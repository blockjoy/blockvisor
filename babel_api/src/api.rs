use crate::config;
use crate::config::SupervisorConfig;
use serde::{Deserialize, Serialize};
use strum_macros::Display;

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
    ApplicationStatus,
    SyncStatus,
    StakingStatus,
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait BabelSup {
    fn get_version() -> String;
    #[server_streaming]
    fn get_logs() -> String;
    fn check_babel(checksum: u32) -> BabelStatus;
    #[client_streaming]
    fn start_new_babel(babel_bin: BabelBin);
    fn setup_supervisor(config: SupervisorConfig);
}

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
pub trait Babel {
    /// Setup firewall according to given configuration.
    fn setup_firewall(config: config::firewall::Config);
    /// Download key files from locations specified in `keys` section of Babel config.
    fn download_keys(config: config::KeysConfig) -> Vec<BlockchainKey>;
    /// Upload files into locations specified in `keys` section of Babel config.
    fn upload_keys(config: config::KeysConfig, keys: Vec<BlockchainKey>) -> BlockchainResponse;

    /// Send a Jrpc request to the current blockchain.
    fn blockchain_jrpc(
        /// This is the host for the JSON rpc request.
        host: String,
        /// The name of the jRPC method that we are going to call into.
        method: String,
        /// This field is ignored.
        response: config::JrpcResponse,
    ) -> BlockchainResponse;

    /// Send a Rest request to the current blockchain.
    fn blockchain_rest(
        /// This is the url of the rest endpoint.
        url: String,
        /// These are the configuration options for parsing the response.
        response: config::RestResponse,
    ) -> BlockchainResponse;

    /// Send a Sh request to the current blockchain.
    fn blockchain_sh(
        /// These are the arguments to the sh command that is executed for this `Method`.
        body: String,
        /// These are the configuration options for parsing the response.
        response: config::ShResponse,
    ) -> BlockchainResponse;
}

pub type BlockchainResponse = Result<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockchainKey {
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub content: Vec<u8>,
}
