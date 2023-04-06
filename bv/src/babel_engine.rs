use crate::{node_connection::NodeConnection, node_data::NodeProperties, render, with_retry};
use anyhow::{anyhow, bail, Context, Result};
use babel_api::config::{
    Babel, KeysConfig,
    Method::{Jrpc, Rest, Sh},
};
use futures_util::StreamExt;
use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::{Debug, Display},
    str::FromStr,
};
use tracing::instrument;
use uuid::Uuid;

#[derive(Debug)]
pub struct BabelEngine {
    node_id: Uuid,
    pub node_conn: NodeConnection,
    babel_conf: Babel,
    properties: NodeProperties,
}

impl BabelEngine {
    pub fn new(
        node_id: Uuid,
        node_conn: NodeConnection,
        babel_conf: Babel,
        properties: NodeProperties,
    ) -> Self {
        Self {
            node_id,
            node_conn,
            babel_conf,
            properties,
        }
    }

    /// Returns the height of the blockchain (in blocks).
    pub async fn height(&mut self) -> Result<u64> {
        self.call_method(babel_api::BabelMethod::Height, HashMap::new())
            .await
    }

    /// Returns the block age of the blockchain (in seconds).
    pub async fn block_age(&mut self) -> Result<u64> {
        self.call_method(babel_api::BabelMethod::BlockAge, HashMap::new())
            .await
    }

    /// Returns the name of the node. This is usually some random generated name that you may use
    /// to recognise the node, but the purpose may vary per blockchain.
    /// ### Example
    /// `chilly-peach-kangaroo`
    pub async fn name(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::Name, HashMap::new())
            .await
    }

    /// The address of the node. The meaning of this varies from blockchain to blockchain.
    /// ### Example
    /// `/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3`
    pub async fn address(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::Address, HashMap::new())
            .await
    }

    /// Returns whether this node is in consensus or not.
    pub async fn consensus(&mut self) -> Result<bool> {
        self.call_method(babel_api::BabelMethod::Consensus, HashMap::new())
            .await
    }

    pub async fn application_status(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::ApplicationStatus, HashMap::new())
            .await
    }

    pub async fn sync_status(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::SyncStatus, HashMap::new())
            .await
    }

    pub async fn staking_status(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::StakingStatus, HashMap::new())
            .await
    }

    pub async fn init(&mut self, secret_keys: HashMap<String, Vec<u8>>) -> Result<String> {
        let mut node_keys = self
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), vec![v.clone()]))
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
                    entry.insert(vec![String::from_utf8(v)?]);
                }
            }
        }

        self.call_method(babel_api::BabelMethod::Init, node_keys)
            .await
    }

    /// This function calls babel by sending a blockchain command using the specified method name.
    #[instrument(skip(self), fields(id = %self.node_id, name = name.to_string()), err, ret(Debug))]
    pub async fn call_method<T>(
        &mut self,
        name: impl Display,
        params: HashMap<String, Vec<String>>,
    ) -> Result<T>
    where
        T: FromStr + Debug,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        let get_api_host = |method: &str| -> Result<&String> {
            self.babel_conf
                .config
                .api_host
                .as_ref()
                .ok_or_else(|| anyhow!("No host specified for method `{method}"))
        };
        let join_params = |v: &Vec<String>| Ok(v.join(","));
        let conf = toml::Value::try_from(&self.babel_conf)?;
        let babel_client = self.node_conn.babel_client().await?;
        let resp = match self
            .babel_conf
            .methods
            .get(&name.to_string())
            .ok_or_else(|| anyhow!("method `{name}` not found"))?
        {
            Jrpc {
                method, response, ..
            } => {
                with_retry!(babel_client.blockchain_jrpc((
                    get_api_host(method)?.clone(),
                    render::render_with(method, &params, &conf, join_params)?,
                    response.clone(),
                )))
            }
            Rest {
                method, response, ..
            } => {
                let host = get_api_host(method)?;

                let url = format!(
                    "{}/{}",
                    host.trim_end_matches('/'),
                    method.trim_start_matches('/')
                );

                with_retry!(babel_client.blockchain_rest((
                    render::render_with(&url, &params, &conf, join_params)?,
                    response.clone(),
                )))
            }
            Sh { body, response, .. } => {
                with_retry!(babel_client.blockchain_sh((
                    render::render_with(body, &params, &conf, render::render_sh_param)?,
                    response.clone(),
                )))
            }
        };
        let value = resp
            .map_err(|err| match err.code() {
                // just forward internal errors
                tonic::Code::Internal => err,
                _ => {
                    // for others mark connection as broken
                    self.node_conn.mark_broken();
                    err
                }
            })?
            .into_inner();
        value
            .trim()
            .parse()
            .context(format!("Could not parse {name} response: {value}"))
    }

    /// Returns the methods that are supported by this blockchain. Calling any method on this
    /// blockchain that is not listed here will result in an error being returned.
    pub fn capabilities(&self) -> Vec<String> {
        self.babel_conf
            .methods
            .keys()
            .map(|method| method.to_string())
            .collect()
    }

    /// Checks if node has some particular capability
    pub fn has_capability(&self, method: &str) -> bool {
        self.capabilities().contains(&method.to_owned())
    }

    /// Returns the list of logs from blockchain jobs.
    pub async fn get_logs(&mut self) -> Result<Vec<String>> {
        let client = self.node_conn.babel_client().await?;
        let mut resp = with_retry!(client.get_logs(()))?.into_inner();
        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = resp.next().await {
            logs.push(log);
        }
        Ok(logs)
    }

    /// Returns blockchain node keys.
    pub async fn download_keys(&mut self) -> Result<Vec<babel_api::BlockchainKey>> {
        let config = self.get_keys_config()?;
        let babel_client = self.node_conn.babel_client().await?;
        let keys = with_retry!(babel_client.download_keys(config.clone()))?.into_inner();
        Ok(keys)
    }

    /// Sets blockchain node keys.
    pub async fn upload_keys(&mut self, keys: Vec<babel_api::BlockchainKey>) -> Result<()> {
        let config = self.get_keys_config()?;
        let babel_client = self.node_conn.babel_client().await?;
        with_retry!(babel_client.upload_keys((config.clone(), keys.clone())))?;
        Ok(())
    }

    fn get_keys_config(&self) -> Result<KeysConfig> {
        self.babel_conf
            .keys
            .clone()
            .ok_or_else(|| anyhow!("No `keys` section found in config"))
    }

    /// Generates keys on node
    pub async fn generate_keys(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::GenerateKeys, HashMap::new())
            .await
    }
}
