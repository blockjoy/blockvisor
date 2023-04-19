/// This module wraps all Babel related functionality. In particular it implements binding between
/// Babel Plugin and Babel running on the node.
///
/// Since Babel Plugin may incorporate external scripting language (like Rhai) that doesn't support
/// async model, it is needed to implement Sync Plugin to Async BV code binding. It is done by running
/// each operation on Plugin in separate thread (by `tokio::task::spawn_blocking()`), see `on_plugin`.
/// Then all requests to `Engine` are translated to messages, then sent with `tokio::sync::mpsc`,
/// and then result is sent back with `tokio::sync::oneshot`, see `handle_node_req`. That's why all
/// Engine methods that implementation needs to interact with node via BV are sent as `NodeRequest`.
/// `BabelEngine` handle all that messages until parallel operation on Plugin is finished.
use crate::{node_data::NodeProperties, with_retry};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use babel_api::{
    babel::babel_client::BabelClient,
    engine::{JobConfig, JobStatus},
    metadata::KeysConfig,
    plugin::{ApplicationStatus, Plugin, StakingStatus, SyncStatus},
};
use bv_utils::run_flag::RunFlag;
use futures_util::StreamExt;
use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio::select;
use tonic::{transport::Channel, Status};
use tracing::instrument;
use uuid::Uuid;

#[async_trait]
pub trait BabelConnection {
    async fn babel_client(&mut self) -> Result<&mut BabelClient<Channel>>;
    fn mark_broken(&mut self);
}

#[derive(Debug)]
pub struct BabelEngine<B, P> {
    node_id: Uuid,
    babel_connection: B,
    properties: NodeProperties,
    plugin: P,
    node_rx: tokio::sync::mpsc::Receiver<NodeRequest>,
}

impl<B: BabelConnection, P: Plugin + Clone + Send + 'static> BabelEngine<B, P> {
    pub fn new<F: FnOnce(Engine) -> P>(
        node_id: Uuid,
        babel_connection: B,
        plugin_builder: F,
        properties: NodeProperties,
        plugin_data_path: PathBuf,
    ) -> Result<Self> {
        let (node_tx, node_rx) = tokio::sync::mpsc::channel(16);
        let engine = Engine {
            tx: node_tx,
            params: properties.clone(),
            plugin_data_path,
        };
        Ok(Self {
            node_id,
            babel_connection,
            plugin: plugin_builder(engine),
            properties,
            node_rx,
        })
    }

    /// Returns the height of the blockchain (in blocks).
    pub async fn height(&mut self) -> Result<u64> {
        self.on_plugin(|plugin| plugin.height()).await
    }

    /// Returns the block age of the blockchain (in seconds).
    pub async fn block_age(&mut self) -> Result<u64> {
        self.on_plugin(|plugin| plugin.block_age()).await
    }

    /// Returns the name of the node. This is usually some random generated name that you may use
    /// to recognise the node, but the purpose may vary per blockchain.
    /// ### Example
    /// `chilly-peach-kangaroo`
    pub async fn name(&mut self) -> Result<String> {
        self.on_plugin(|plugin| plugin.name()).await
    }

    /// The address of the node. The meaning of this varies from blockchain to blockchain.
    /// ### Example
    /// `/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3`
    pub async fn address(&mut self) -> Result<String> {
        self.on_plugin(|plugin| plugin.address()).await
    }

    /// Returns whether this node is in consensus or not.
    pub async fn consensus(&mut self) -> Result<bool> {
        self.on_plugin(|plugin| plugin.consensus()).await
    }

    pub async fn application_status(&mut self) -> Result<ApplicationStatus> {
        self.on_plugin(|plugin| plugin.application_status()).await
    }

    pub async fn sync_status(&mut self) -> Result<SyncStatus> {
        self.on_plugin(|plugin| plugin.sync_status()).await
    }

    pub async fn staking_status(&mut self) -> Result<StakingStatus> {
        self.on_plugin(|plugin| plugin.staking_status()).await
    }

    pub async fn init(&mut self, secret_keys: HashMap<String, Vec<u8>>) -> Result<()> {
        let mut node_keys = self
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<HashMap<_, _>>();

        for (k, v) in secret_keys.into_iter() {
            match node_keys.entry(k) {
                Entry::Occupied(entry) => {
                    // A parameter named "KEY" that comes form the backend or possibly from a user
                    // must not have the same name as any of the parameters in node_keys map. This
                    // can lead to undefined behaviour or even a security issue. A user-provided
                    // value could be (unintentionally) joined with a node_keys value into a single
                    // parameter and used in an undesirable way. Since the user has no way to
                    // define parameter names this must be treated as internal error (that should
                    // never happen, but ...).
                    bail!("Secret keys KEY collides with params KEY: {}", entry.key())
                }
                Entry::Vacant(entry) => {
                    entry.insert(String::from_utf8(v)?);
                }
            }
        }
        self.on_plugin(move |plugin| plugin.init(&node_keys)).await
    }

    /// This function calls babel by sending a blockchain command using the specified method name.
    #[instrument(skip(self), fields(id = % self.node_id, name = name.to_string()), err, ret(Debug))]
    pub async fn call_method<T>(&mut self, name: &str, param: &str) -> Result<T>
    where
        T: FromStr + Debug,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        let method_name = name.to_owned();
        let method_param = param.to_owned();
        let value = self
            .on_plugin(move |plugin| plugin.call_custom_method(&method_name, &method_param))
            .await?;
        value
            .trim()
            .parse()
            .context(format!("Could not parse {name} response: {value}"))
    }

    /// Returns the methods that are supported by this blockchain. Calling any method on this
    /// blockchain that is not listed here will result in an error being returned.
    pub async fn capabilities(&mut self) -> Result<Vec<String>> {
        self.on_plugin(move |plugin| Ok(plugin.capabilities()))
            .await
    }

    /// Checks if node has some particular capability
    pub async fn has_capability(&mut self, method: &str) -> Result<bool> {
        let method = method.to_owned();
        self.on_plugin(move |plugin| Ok(plugin.has_capability(&method)))
            .await
    }

    /// Returns the list of logs from blockchain jobs.
    pub async fn get_logs(&mut self) -> Result<Vec<String>> {
        let client = self.babel_connection.babel_client().await?;
        let mut resp = with_retry!(client.get_logs(()))?.into_inner();
        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = resp.next().await {
            logs.push(log);
        }
        Ok(logs)
    }

    /// Returns blockchain node keys.
    pub async fn download_keys(&mut self) -> Result<Vec<babel_api::babel::BlockchainKey>> {
        let config = self.get_keys_config().await?;
        let babel_client = self.babel_connection.babel_client().await?;
        let keys = with_retry!(babel_client.download_keys(config.clone()))?.into_inner();
        Ok(keys)
    }

    /// Sets blockchain node keys.
    pub async fn upload_keys(&mut self, keys: Vec<babel_api::babel::BlockchainKey>) -> Result<()> {
        let config = self.get_keys_config().await?;
        let babel_client = self.babel_connection.babel_client().await?;
        with_retry!(babel_client.upload_keys((config.clone(), keys.clone())))?;
        Ok(())
    }

    async fn get_keys_config(&mut self) -> Result<KeysConfig> {
        self.on_plugin(move |plugin| plugin.metadata())
            .await?
            .keys
            .ok_or_else(|| anyhow!("No `keys` section found in metadata"))
    }

    /// Generates keys on node
    pub async fn generate_keys(&mut self) -> Result<()> {
        self.on_plugin(|plugin| plugin.generate_keys()).await
    }

    /// Clone plugin, move it to separate thread and call given function `f` on it.
    /// In parallel it run `node_request_handler` until function on plugin is done.
    async fn on_plugin<T: Send + 'static, F: FnOnce(P) -> Result<T> + Send + 'static>(
        &mut self,
        f: F,
    ) -> Result<T> {
        let plugin = self.plugin.clone();
        let mut run = RunFlag::default();
        let handler_run = run.clone();
        let (resp, _) = tokio::join!(
            tokio::task::spawn_blocking(move || {
                let res = f(plugin);
                run.stop();
                res
            }),
            self.node_request_handler(handler_run)
        );
        resp?
    }

    /// Listen for `NodeRequest`'s, handle them and send results back to plugin.
    async fn node_request_handler(&mut self, mut run: RunFlag) {
        while run.load() {
            select!(
                req = self.node_rx.recv() => {
                    if let Some(req) = req {
                        self.handle_node_req(req).await;
                    }
                }
                _ = run.wait() => {}
            )
        }
    }

    async fn handle_node_req(&mut self, req: NodeRequest) {
        let babel_client = self.babel_connection.babel_client().await;
        match req {
            NodeRequest::RunSh { body, response_tx } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => with_retry!(babel_client.run_sh(body.clone()))
                        .map_err(|err| self.handle_connection_errors(err))
                        .map(|v| v.into_inner()),
                    Err(err) => Err(err),
                });
            }
            NodeRequest::RunRest { url, response_tx } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => with_retry!(babel_client.run_rest(url.clone()))
                        .map_err(|err| self.handle_connection_errors(err))
                        .map(|v| v.into_inner()),
                    Err(err) => Err(err),
                });
            }
            NodeRequest::RunJrpc {
                host,
                method,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => {
                        with_retry!(babel_client.run_jrpc((host.clone(), method.clone())))
                            .map_err(|err| self.handle_connection_errors(err))
                            .map(|v| v.into_inner())
                    }
                    Err(err) => Err(err),
                });
            }
            NodeRequest::StartJob {
                job_name,
                job_config,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => {
                        with_retry!(babel_client.start_job((job_name.clone(), job_config.clone())))
                            .map_err(|err| self.handle_connection_errors(err))
                            .map(|v| v.into_inner())
                    }
                    Err(err) => Err(err),
                });
            }
            NodeRequest::StopJob {
                job_name,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => with_retry!(babel_client.stop_job(job_name.clone()))
                        .map_err(|err| self.handle_connection_errors(err))
                        .map(|v| v.into_inner()),
                    Err(err) => Err(err),
                });
            }
            NodeRequest::JobStatus {
                job_name,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => with_retry!(babel_client.job_status(job_name.clone()))
                        .map_err(|err| self.handle_connection_errors(err))
                        .map(|v| v.into_inner()),
                    Err(err) => Err(err),
                });
            }
            NodeRequest::RenderTemplate {
                template,
                output,
                params,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => with_retry!(babel_client.render_template((
                        template.clone(),
                        output.clone(),
                        params.clone()
                    )))
                    .map_err(|err| self.handle_connection_errors(err))
                    .map(|v| v.into_inner()),
                    Err(err) => Err(err),
                });
            }
        }
    }

    fn handle_connection_errors(&mut self, err: Status) -> anyhow::Error {
        match err.code() {
            // just forward internal errors
            tonic::Code::Internal => err,
            _ => {
                // for others mark connection as broken
                self.babel_connection.mark_broken();
                err
            }
        }
        .into()
    }
}

/// Engine trait implementation. For methods that require interaction with async BV code, it translate
/// function into message that is sent to BV thread and synchronously waits for the response.
#[derive(Debug, Clone)]
pub struct Engine {
    tx: tokio::sync::mpsc::Sender<NodeRequest>,
    params: NodeProperties,
    plugin_data_path: PathBuf,
}

type ResponseTx<T> = tokio::sync::oneshot::Sender<T>;

#[derive(Debug)]
enum NodeRequest {
    StartJob {
        job_name: String,
        job_config: JobConfig,
        response_tx: ResponseTx<Result<()>>,
    },
    StopJob {
        job_name: String,
        response_tx: ResponseTx<Result<()>>,
    },
    JobStatus {
        job_name: String,
        response_tx: ResponseTx<Result<JobStatus>>,
    },
    RunJrpc {
        host: String,
        method: String,
        response_tx: ResponseTx<Result<String>>,
    },
    RunRest {
        url: String,
        response_tx: ResponseTx<Result<String>>,
    },
    RunSh {
        body: String,
        response_tx: ResponseTx<Result<String>>,
    },
    RenderTemplate {
        template: PathBuf,
        output: PathBuf,
        params: HashMap<String, String>,
        response_tx: ResponseTx<Result<()>>,
    },
}

impl babel_api::engine::Engine for Engine {
    fn start_job(&self, job_name: &str, job_config: JobConfig) -> Result<()> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::StartJob {
            job_name: job_name.to_string(),
            job_config,
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn stop_job(&self, job_name: &str) -> Result<()> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::StopJob {
            job_name: job_name.to_string(),
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn job_status(&self, job_name: &str) -> Result<JobStatus> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::JobStatus {
            job_name: job_name.to_string(),
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn run_jrpc(&self, host: &str, method: &str) -> Result<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RunJrpc {
            host: host.to_string(),
            method: method.to_string(),
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn run_rest(&self, url: &str) -> Result<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RunRest {
            url: url.to_string(),
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn run_sh(&self, body: &str) -> Result<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RunSh {
            body: body.to_string(),
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn sanitize_sh_param(&self, param: &str) -> Result<String> {
        Ok(format!(
            "\"{}\"",
            param
                .chars()
                .map(escape_sh_char)
                .collect::<Result<String>>()?
        ))
    }

    fn render_template(
        &self,
        template: &Path,
        output: &Path,
        params: HashMap<String, String>,
    ) -> Result<()> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RenderTemplate {
            template: template.to_path_buf(),
            output: output.to_path_buf(),
            params,
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn node_params(&self) -> HashMap<String, String> {
        self.params.clone()
    }

    fn save_data(&self, value: &str) -> Result<()> {
        Ok(fs::write(&self.plugin_data_path, value)?)
    }

    fn load_data(&self) -> Result<String> {
        Ok(fs::read_to_string(&self.plugin_data_path)?)
    }
}

/// If the character is allowed, escapes a character into something we can use for a
/// bash-substitution.
fn escape_sh_char(c: char) -> Result<String> {
    match c {
        // Explicit disallowance of ', since that is the delimiter we use in `render_config`.
        '\'' => bail!("Very unsafe subsitution >:( {c}"),
        // Alphanumerics do not need escaping.
        _ if c.is_alphanumeric() => Ok(c.to_string()),
        // Quotes need to be escaped.
        '"' => Ok("\\\"".to_string()),
        // Newlines must be esacped
        '\n' => Ok("\\n".to_string()),
        // These are the special characters we allow that do not need esacping.
        '/' | ':' | '{' | '}' | ',' | '-' | '_' | '.' | ' ' => Ok(c.to_string()),
        // If none of these cases match, we return an error.
        c => bail!("Shell unsafe character detected: {c}"),
    }
}
