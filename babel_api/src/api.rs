use crate::config;
use crate::config::SupervisorConfig;
use serde::{Deserialize, Serialize};
use std::collections;
use strum_macros::Display;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BabelStatus {
    Ok,
    ChecksumMismatch,
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BabelBin {
    Bin(Vec<u8>),
    Checksum(u32),
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait BabelSup {
    #[server_streaming]
    fn get_logs() -> String;
    fn check_babel(checksum: u32) -> BabelStatus;
    #[client_streaming]
    fn start_new_babel(babel_bin: BabelBin);
    fn setup_supervisor(config: SupervisorConfig);
}

/// Each request that comes over the VSock to babel must be a piece of JSON that can be
/// deserialized into this struct.
#[derive(Debug, Serialize, Deserialize)]
pub enum BabelRequest {
    /// Returns `Pong`. Useful to check for the liveness of the node.
    Ping,
    /// Send a Jrpc request to the current blockchain.
    BlockchainJrpc {
        /// This is the host for the JSON rpc request.
        host: String,
        /// The name of the jRPC method that we are going to call into.
        method: String,
        /// This field is ignored.
        response: config::JrpcResponse,
    },
    /// Send a Rest request to the current blockchain.
    BlockchainRest {
        /// This is the url of the rest endpoint.
        url: String,
        /// These are the configuration options for parsing the response.
        response: config::RestResponse,
    },
    /// Send a Sh request to the current blockchain.
    BlockchainSh {
        /// These are the arguments to the sh command that is executed for this `Method`.
        body: String,
        /// These are the configuration options for parsing the response.
        response: config::ShResponse,
    },
    /// Download key files from locations specified in `keys` section of Babel config.
    DownloadKeys(config::KeysConfig),
    /// Upload files into locations specified in `keys` section of Babel config.
    UploadKeys {
        config: config::KeysConfig,
        keys: Vec<BlockchainKey>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockchainCommand {
    pub name: String,
    pub params: BlockchainParams,
}

pub type BlockchainParams = collections::HashMap<String, Vec<String>>;

#[derive(Debug, Serialize, Deserialize)]
pub enum BabelResponse {
    Pong,
    BlockchainResponse(BlockchainResponse),
    Keys(Vec<BlockchainKey>),
    Error(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockchainResponse {
    pub value: String,
}

impl From<String> for BlockchainResponse {
    fn from(value: String) -> Self {
        Self { value }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockchainKey {
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub content: Vec<u8>,
}

#[derive(Debug, Display)]
#[strum(serialize_all = "snake_case")]
pub enum BabelMethod {
    Init,
    Address,
    BlockAge,
    Consensus,
    GenerateKeys,
    Height,
    Name,
}
