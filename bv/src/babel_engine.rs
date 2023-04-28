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
use crate::{
    node_connection::{BabelConnection, RPC_REQUEST_TIMEOUT},
    node_data::NodeProperties,
    with_retry,
};
use anyhow::{anyhow, bail, Result};
use babel_api::{
    engine::{JobConfig, JobStatus},
    metadata::KeysConfig,
    plugin::{ApplicationStatus, Plugin, StakingStatus, SyncStatus},
};
use bv_utils::run_flag::RunFlag;
use futures_util::StreamExt;
use std::time::Duration;
use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
};
use tokio::select;
use tonic::{Request, Status};
use tracing::instrument;
use uuid::Uuid;

#[macro_export]
macro_rules! with_retry_on_conn_error {
    ($fun:expr) => {{
        const RPC_RETRY_MAX: u32 = 3;
        const RPC_BACKOFF_BASE_MSEC: u64 = 300;
        let mut retry_count = 0;
        loop {
            match $fun.await {
                Ok(res) => break Ok(res),
                Err(err) if err.code() != tonic::Code::Internal => {
                    if retry_count < RPC_RETRY_MAX {
                        retry_count += 1;
                        let backoff = RPC_BACKOFF_BASE_MSEC * 2u64.pow(retry_count);
                        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                        continue;
                    } else {
                        break Err(err);
                    }
                }
                Err(err) => {
                    break Err(err);
                }
            }
        }
    }};
}

#[derive(Debug)]
pub struct BabelEngine<B, P> {
    node_id: Uuid,
    pub babel_connection: B,
    properties: NodeProperties,
    plugin: P,
    plugin_data_path: PathBuf,
    node_rx: tokio::sync::mpsc::Receiver<NodeRequest>,
    node_tx: tokio::sync::mpsc::Sender<NodeRequest>,
}

impl<B: BabelConnection, P: Plugin + Clone + Send + 'static> BabelEngine<B, P> {
    pub fn new<F: FnOnce(Engine) -> Result<P>>(
        node_id: Uuid,
        babel_connection: B,
        plugin_builder: F,
        properties: NodeProperties,
        plugin_data_path: PathBuf,
    ) -> Result<Self> {
        let (node_tx, node_rx) = tokio::sync::mpsc::channel(16);
        let engine = Engine {
            tx: node_tx.clone(),
            params: properties.clone(),
            plugin_data_path: plugin_data_path.clone(),
        };
        Ok(Self {
            node_id,
            babel_connection,
            plugin: plugin_builder(engine)?,
            properties,
            plugin_data_path,
            node_rx,
            node_tx,
        })
    }

    pub fn update_plugin<F: FnOnce(Engine) -> Result<P>>(
        &mut self,
        plugin_builder: F,
    ) -> Result<()> {
        let engine = Engine {
            tx: self.node_tx.clone(),
            params: self.properties.clone(),
            plugin_data_path: self.plugin_data_path.clone(),
        };
        self.plugin = plugin_builder(engine)?;
        Ok(())
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
    pub async fn call_method(&mut self, name: &str, param: &str) -> Result<String> {
        Ok(match name {
            "init" => {
                self.init(serde_json::from_str(param)?).await?;
                Default::default()
            }
            "height" => self.height().await?.to_string(),
            "block_age" => self.block_age().await?.to_string(),
            "name" => self.name().await?,
            "address" => self.address().await?,
            "consensus" => self.consensus().await?.to_string(),
            "application_status" => serde_json::to_string(&self.application_status().await?)?,
            "sync_status" => serde_json::to_string(&self.sync_status().await?)?,
            "staking_status" => serde_json::to_string(&self.staking_status().await?)?,
            "generate_keys" => {
                self.generate_keys().await?;
                Default::default()
            }
            _ => {
                let method_name = name.to_owned();
                let method_param = param.to_owned();
                self.on_plugin(move |plugin| plugin.call_custom_method(&method_name, &method_param))
                    .await?
            }
        })
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
            NodeRequest::RunSh {
                body,
                timeout,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => with_retry_on_conn_error!(babel_client.run_sh(
                        with_timeout(body.clone(), timeout.unwrap_or(RPC_REQUEST_TIMEOUT))
                    ))
                    .map_err(|err| self.handle_connection_errors(err))
                    .map(|v| v.into_inner()),
                    Err(err) => Err(err),
                });
            }
            NodeRequest::RunRest {
                url,
                timeout,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => with_retry_on_conn_error!(babel_client.run_rest(
                        with_timeout(url.clone(), timeout.unwrap_or(RPC_REQUEST_TIMEOUT))
                    ))
                    .map_err(|err| self.handle_connection_errors(err))
                    .map(|v| v.into_inner()),
                    Err(err) => Err(err),
                });
            }
            NodeRequest::RunJrpc {
                host,
                method,
                timeout,
                response_tx,
            } => {
                let _ = response_tx.send(match babel_client {
                    Ok(babel_client) => {
                        with_retry_on_conn_error!(babel_client.run_jrpc(with_timeout(
                            (host.clone(), method.clone()),
                            timeout.unwrap_or(RPC_REQUEST_TIMEOUT)
                        )))
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

fn with_timeout<T>(args: T, timeout: Duration) -> Request<T> {
    let mut req = Request::new(args);
    req.set_timeout(timeout);
    req
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
        timeout: Option<Duration>,
        response_tx: ResponseTx<Result<String>>,
    },
    RunRest {
        url: String,
        timeout: Option<Duration>,
        response_tx: ResponseTx<Result<String>>,
    },
    RunSh {
        body: String,
        timeout: Option<Duration>,
        response_tx: ResponseTx<Result<String>>,
    },
    RenderTemplate {
        template: PathBuf,
        output: PathBuf,
        params: String,
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

    fn run_jrpc(&self, host: &str, method: &str, timeout: Option<Duration>) -> Result<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RunJrpc {
            host: host.to_string(),
            method: method.to_string(),
            timeout,
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn run_rest(&self, url: &str, timeout: Option<Duration>) -> Result<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RunRest {
            url: url.to_string(),
            timeout,
            response_tx,
        })?;
        response_rx.blocking_recv()?
    }

    fn run_sh(&self, body: &str, timeout: Option<Duration>) -> Result<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RunSh {
            body: body.to_string(),
            timeout,
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

    fn render_template(&self, template: &Path, output: &Path, params: &str) -> Result<()> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.tx.blocking_send(NodeRequest::RenderTemplate {
            template: template.to_path_buf(),
            output: output.to_path_buf(),
            params: params.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_connection::{BabelClient, DefaultTimeout};
    use crate::utils;
    use crate::utils::tests::test_channel;
    use assert_fs::TempDir;
    use async_trait::async_trait;
    use babel_api::engine::{Engine, RestartPolicy};
    use babel_api::{babel::BlockchainKey, metadata::BabelConfig};
    use mockall::*;
    use tonic::{Request, Response, Streaming};

    mock! {
        pub BabelService {}

        #[tonic::async_trait]
        impl babel_api::babel::babel_server::Babel for BabelService {
            async fn setup_babel(
                &self,
                request: Request<(String, BabelConfig)>,
            ) -> Result<Response<()>, Status>;
            async fn setup_firewall(
                &self,
                request: Request<babel_api::metadata::firewall::Config>,
            ) -> Result<Response<()>, Status>;
            async fn download_keys(
                &self,
                request: Request<babel_api::metadata::KeysConfig>,
            ) -> Result<Response<Vec<BlockchainKey>>, Status>;
            async fn upload_keys(
                &self,
                request: Request<(babel_api::metadata::KeysConfig, Vec<BlockchainKey>)>,
            ) -> Result<Response<String>, Status>;
            async fn check_job_runner(
                &self,
                request: Request<u32>,
            ) -> Result<Response<babel_api::utils::BinaryStatus>, Status>;
            async fn upload_job_runner(
                &self,
                request: Request<Streaming<babel_api::utils::Binary>>,
            ) -> Result<Response<()>, Status>;
            async fn start_job(
                &self,
                request: Request<(String, JobConfig)>,
            ) -> Result<Response<()>, Status>;
            async fn stop_job(&self, request: Request<String>) -> Result<Response<()>, Status>;
            async fn job_status(&self, request: Request<String>) -> Result<Response<JobStatus>, Status>;
            async fn run_jrpc(
                &self,
                request: Request<(String, String)>,
            ) -> Result<Response<String>, Status>;
            async fn run_rest(
                &self,
                request: Request<String>,
            ) -> Result<Response<String>, Status>;
            async fn run_sh(
                &self,
                request: Request<String>,
            ) -> Result<Response<String>, Status>;
            async fn render_template(
                &self,
                request: Request<(PathBuf, PathBuf, String)>,
            ) -> Result<Response<()>, Status>;
            type GetLogsStream = tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>;
            async fn get_logs(
                &self,
                _request: Request<()>,
            ) -> Result<Response<tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>>, Status>;
        }
    }

    #[derive(Clone)]
    struct DummyPlugin {
        engine: super::Engine,
    }

    impl Plugin for DummyPlugin {
        fn metadata(&self) -> Result<babel_api::metadata::BlockchainMetadata> {
            self.engine.run_sh("metadata", None)?;
            bail!("no metadata") // test also some error propagation
        }
        fn capabilities(&self) -> Vec<String> {
            self.engine.run_sh("capabilities", None).unwrap();
            vec!["some_method".to_string()]
        }
        fn has_capability(&self, _name: &str) -> bool {
            self.engine.run_sh("has_capability", None).unwrap();
            true
        }
        fn init(&self, secret_keys: &HashMap<String, String>) -> Result<()> {
            self.engine.render_template(
                Path::new("template"),
                Path::new("config"),
                &serde_json::to_string(secret_keys)?,
            )?;
            Ok(())
        }
        fn height(&self) -> Result<u64> {
            self.engine.run_sh("height", None)?;
            Ok(7)
        }
        fn block_age(&self) -> Result<u64> {
            self.engine.run_sh("block_age", None)?;
            Ok(77)
        }
        fn name(&self) -> Result<String> {
            self.engine.run_sh("dummy_name", None)
        }
        fn address(&self) -> Result<String> {
            self.engine.run_sh("dummy address", None)
        }
        fn consensus(&self) -> Result<bool> {
            self.engine.run_sh("consensus", None)?;
            Ok(true)
        }
        fn application_status(&self) -> Result<ApplicationStatus> {
            self.engine.run_sh("application_status", None)?;
            Ok(ApplicationStatus::Disabled)
        }
        fn sync_status(&self) -> Result<SyncStatus> {
            self.engine.run_sh("sync_status", None)?;
            Ok(SyncStatus::Syncing)
        }
        fn staking_status(&self) -> Result<StakingStatus> {
            self.engine.run_sh("staking_status", None)?;
            Ok(StakingStatus::Staked)
        }
        fn generate_keys(&self) -> Result<()> {
            self.engine.run_sh("generate_keys", None)?;
            Ok(())
        }
        fn call_custom_method(&self, name: &str, param: &str) -> Result<String> {
            self.engine.start_job(
                name,
                JobConfig {
                    body: param.to_string(),
                    restart: RestartPolicy::Never,
                    needs: None,
                },
            )?;
            self.engine.stop_job(name)?;
            self.engine.job_status(name)?;
            self.engine.run_jrpc(name, param, None)?;
            self.engine.run_rest(name, None)?;
            self.engine.render_template(
                Path::new(name),
                Path::new(param),
                &serde_json::to_string(&self.engine.node_params())?,
            )?;
            self.engine.save_data("custom plugin data")?;
            self.engine.load_data()
        }
    }

    struct TestConnection {
        client: BabelClient,
    }

    #[async_trait]
    impl BabelConnection for TestConnection {
        async fn babel_client(&mut self) -> Result<&mut BabelClient> {
            Ok(&mut self.client)
        }

        fn mark_broken(&mut self) {}
    }

    /// Common staff to setup for all tests like sut (BabelEngine in that case),
    /// path to root dir used in test, instance of AsyncPanicChecker to make sure that all panics
    /// from other threads will be propagated.
    struct TestEnv {
        tmp_root: PathBuf,
        data_path: PathBuf,
        engine: BabelEngine<TestConnection, DummyPlugin>,
        _async_panic_checker: utils::tests::AsyncPanicChecker,
    }

    impl TestEnv {
        fn new() -> Result<Self> {
            let tmp_root = TempDir::new()?.to_path_buf();
            fs::create_dir_all(&tmp_root)?;
            let data_path = tmp_root.join("data");
            let connection = TestConnection {
                client: babel_api::babel::babel_client::BabelClient::with_interceptor(
                    test_channel(&tmp_root),
                    DefaultTimeout,
                ),
            };
            let engine = BabelEngine::new(
                Uuid::new_v4(),
                connection,
                |engine| Ok(DummyPlugin { engine }),
                HashMap::from_iter([("some_key".to_string(), "some value".to_string())]),
                data_path.clone(),
            )?;

            Ok(Self {
                tmp_root,
                data_path,
                engine,
                _async_panic_checker: Default::default(),
            })
        }

        fn start_test_server(&self, babel_mock: MockBabelService) -> utils::tests::TestServer {
            utils::tests::start_test_server(
                self.tmp_root.join("test_socket"),
                babel_api::babel::babel_server::BabelServer::new(babel_mock),
            )
        }
    }

    #[tokio::test]
    async fn test_async_bridge_to_babel() -> Result<()> {
        let mut test_env = TestEnv::new()?;
        let mut babel_mock = MockBabelService::new();
        // from init
        babel_mock
            .expect_render_template()
            .withf(|req| {
                let (template, out, params) = req.get_ref();
                let json: serde_json::Value = serde_json::from_str(params).unwrap();
                let json = json.as_object().unwrap();
                template == Path::new("template")
                    && out == Path::new("config")
                    && json["some_key"].to_string() == "\"some value\""
                    && json["secret_key"].to_string() == "\"\\u0001\\u0002\\u0003\""
            })
            .return_once(|_| Ok(Response::new(())));
        // from custom_method
        babel_mock
            .expect_run_sh()
            .once()
            .withf(|req| req.get_ref() == "dummy_name")
            .returning(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_start_job()
            .withf(|req| {
                let (name, config) = req.get_ref();
                name == "custom_name" && config.body == "param"
            })
            .return_once(|_| Ok(Response::new(())));
        babel_mock
            .expect_stop_job()
            .withf(|req| req.get_ref() == "custom_name")
            .return_once(|_| Ok(Response::new(())));
        babel_mock
            .expect_job_status()
            .withf(|req| req.get_ref() == "custom_name")
            .return_once(|_| Ok(Response::new(JobStatus::Running)));
        babel_mock
            .expect_run_jrpc()
            .withf(|req| {
                let (name, param) = req.get_ref();
                name == "custom_name" && param == "param"
            })
            .return_once(|_| Ok(Response::new("any".to_string())));
        babel_mock
            .expect_run_rest()
            .withf(|req| req.get_ref() == "custom_name")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_render_template()
            .withf(|req| {
                let (template, out, params) = req.get_ref();
                template == Path::new("custom_name")
                    && out == Path::new("param")
                    && params == r#"{"some_key":"some value"}"#
            })
            .return_once(|_| Ok(Response::new(())));

        // others
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "height")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "block_age")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .once()
            .withf(|req| req.get_ref() == "dummy_name")
            .returning(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "dummy address")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "consensus")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "application_status")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "sync_status")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "staking_status")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "capabilities")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "has_capability")
            .return_once(|req| Ok(Response::new(req.into_inner())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "metadata")
            .return_once(|req| Ok(Response::new(req.into_inner())));

        let babel_server = test_env.start_test_server(babel_mock);

        test_env
            .engine
            .init(HashMap::from_iter([(
                "secret_key".to_string(),
                vec![1, 2, 3],
            )]))
            .await?;
        assert_eq!(
            "dummy_name",
            test_env.engine.call_method("name", "param").await?
        );
        assert_eq!(
            "custom plugin data",
            test_env.engine.call_method("custom_name", "param").await?
        );
        assert_eq!(
            "custom plugin data",
            fs::read_to_string(test_env.data_path)?
        );
        assert_eq!(7, test_env.engine.height().await?);
        assert_eq!(77, test_env.engine.block_age().await?);
        assert_eq!("dummy_name", test_env.engine.name().await?);
        assert_eq!("dummy address", test_env.engine.address().await?);
        assert!(test_env.engine.consensus().await?);
        assert_eq!(
            ApplicationStatus::Disabled,
            test_env.engine.application_status().await?
        );
        assert_eq!(SyncStatus::Syncing, test_env.engine.sync_status().await?);
        assert_eq!(
            StakingStatus::Staked,
            test_env.engine.staking_status().await?
        );
        assert_eq!(
            vec!["some_method".to_string()],
            test_env.engine.capabilities().await?
        );
        assert_eq!(true, test_env.engine.has_capability("some method").await?);
        assert_eq!(
            "no metadata",
            test_env
                .engine
                .upload_keys(vec![])
                .await
                .unwrap_err()
                .to_string()
        );
        babel_server.assert().await;

        Ok(())
    }
}
