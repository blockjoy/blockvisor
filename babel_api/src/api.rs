use serde::{Deserialize, Serialize};
use std::collections;
use strum_macros::Display;

/// Each request that comes over the VSock to babel must be a piece of JSON that can be
/// deserialized into this struct.
#[derive(Debug, Serialize, Deserialize)]
pub enum BabelRequest {
    /// Returns `Pong`. Useful to check for the liveness of the node.
    Ping,
    /// List the endpoints that are available for the current blockchain. These are extracted from
    /// the config, and just sent back as strings for now.
    ListCapabilities,
    /// Send a request to the current blockchain. We can identify the way to do this from the
    /// config and forward the provided parameters.
    BlockchainCommand(BlockchainCommand),
    /// Download key files from locations specified in `keys` section of Babel config.
    DownloadKeys,
    /// Upload files into locations specified in `keys` section of Babel config.
    UploadKeys(Vec<BlockchainKey>),
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
    ListCapabilities(Vec<String>),
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
