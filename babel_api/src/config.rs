use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, collections::HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Babel {
    pub export: Option<Vec<String>>,
    pub env: Option<Env>,
    pub config: Config,
    pub requirements: Requirements,
    pub nets: Vec<NetConfiguration>,
    /// Commands to start blockchain node
    pub supervisor: SupervisorConfig,
    pub keys: Option<HashMap<String, String>>,
    #[serde(deserialize_with = "deserialize_methods")]
    pub methods: BTreeMap<String, Method>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorConfig {
    /// Path to mount data drive to
    pub data_directory_mount_point: String,
    ///  if entry_point stay alive given amount of time (in miliseconds) backof is reset
    pub backoff_timeout_ms: u64,
    /// base time (in miliseconds) for backof, multiplied by consecutive power of 2 each time
    pub backoff_base_ms: u64,
    /// capacity of log buffer (in lines)
    pub log_buffer_capacity_ln: usize,
    pub entry_point: Vec<Entrypoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entrypoint {
    pub command: String,
    pub args: Vec<String>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            data_directory_mount_point: "/blockjoy/miner/data".to_string(),
            backoff_timeout_ms: 60_000,
            backoff_base_ms: 100,
            log_buffer_capacity_ln: 1_000,
            entry_point: vec![],
        }
    }
}

pub fn deserialize_methods<'de, D>(deserializer: D) -> Result<BTreeMap<String, Method>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let methods = Vec::<Method>::deserialize(deserializer)?;
    let map = methods
        .into_iter()
        .map(|m| (m.name().to_string(), m))
        .collect();

    Ok(map)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Env {
    pub path_append: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// A semver version of the babel program, indicating the minimum version of the babel
    /// program that a config file is compatible with.
    pub min_babel_version: String,
    /// A semver version of the blockchain node program.
    pub node_version: String,
    /// Name of the blockchain protocol
    pub protocol: String,
    /// Type of the node (validator, beacon, etc)
    pub node_type: String,
    /// Some description of the node
    pub description: Option<String>,
    /// The url where the node exposes its endpoints. Since the blockchain node is running on the
    /// same OS as babel, this will be a local url (usually). Example: `http://localhost:4467/`.
    pub api_host: Option<String>,
    /// Ports used by blockchain software
    /// Ports visibility will be controlled with some kind of firewall
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirements {
    // Virtual cores to share with VM
    pub vcpu_count: usize,
    // RAM allocated to VM
    pub mem_size_mb: usize,
    // Size of data drive for storing blockchain data (not to be confused with OS drive)
    pub disk_size_gb: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NetType {
    Dev,
    Test,
    Main,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetConfiguration {
    pub name: String,
    pub url: String,
    pub net_type: NetType,
    pub meta: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum Method {
    Jrpc {
        /// This field is ignored.
        name: String,
        /// The name of the jRPC method that we are going to call into.
        method: String,
        /// This field is ignored.
        response: JrpcResponse,
    },
    Rest {
        /// This field is ignored.
        name: String,
        /// This is the relative url of the rest endpoint. So if the host is `"https://api.com/"`,
        /// and the method is `"/v1/users"`, then the url that called is
        /// `"https://api.com/v1/users"`.
        method: String,
        /// These are the configuration options for parsing the response.
        response: RestResponse,
    },
    Sh {
        /// This field is ignored.
        name: String,
        /// These are the arguments to the sh command that is executed for this `Method`.
        body: String,
        /// These are the configuration options for parsing the response.
        response: ShResponse,
    },
}

impl Method {
    pub fn name(&self) -> &str {
        match self {
            Method::Jrpc { name, .. } => name,
            Method::Rest { name, .. } => name,
            Method::Sh { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JrpcResponse {
    pub code: u32,
    pub field: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestResponse {
    pub status: u32,
    pub field: Option<String>,
    pub format: MethodResponseFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShResponse {
    pub status: i32,
    pub format: MethodResponseFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MethodResponseFormat {
    Raw,
    Json,
}
