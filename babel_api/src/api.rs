use crate::config;
use serde::{Deserialize, Serialize};
use std::collections;
use std::collections::HashMap;
use strum_macros::Display;

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
    DownloadKeys(HashMap<String, String>),
    /// Upload files into locations specified in `keys` section of Babel config.
    UploadKeys((HashMap<String, String>, Vec<BlockchainKey>)),
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
