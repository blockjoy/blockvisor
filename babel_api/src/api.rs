use crate::{
    config,
    config::{BabelConfig, SupervisorConfig},
};
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
    fn check_babel(checksum: u32) -> BinaryStatus;
    #[client_streaming]
    fn start_new_babel(babel_bin: Binary);
    fn setup_supervisor(config: SupervisorConfig);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BinaryStatus {
    Ok,
    ChecksumMismatch,
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Binary {
    Bin(Vec<u8>),
    Checksum(u32),
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait Babel {
    /// Initial Babel setup that must be run on node startup Mount data directory.
    fn setup_babel(config: BabelConfig);
    /// Setup firewall according to given configuration.
    fn setup_firewall(config: config::firewall::Config);
    /// Download key files from locations specified in `keys` section of Babel config.
    fn download_keys(config: config::KeysConfig) -> Vec<BlockchainKey>;
    /// Upload files into locations specified in `keys` section of Babel config.
    fn upload_keys(config: config::KeysConfig, keys: Vec<BlockchainKey>) -> String;
    /// Check if JobRunner binary exists and its checksum match expected.
    fn check_job_runner(checksum: u32) -> BinaryStatus;
    /// Upload JobRunner binary, overriding existing one (if any).
    #[client_streaming]
    fn upload_job_runner(babel_bin: Binary);
    /// Start background job with unique name.
    fn start_job(job_name: String, job: config::JobConfig);
    /// Stop background job with given unique name if running.
    fn stop_job(job_name: String);
    /// Get background job status by unique name.
    fn job_status(job_name: String) -> config::JobStatus;

    /// Send a Jrpc request to the current blockchain.
    fn blockchain_jrpc(
        /// This is the host for the JSON rpc request.
        host: String,
        /// The name of the jRPC method that we are going to call into.
        method: String,
        /// This field is ignored.
        response: config::JrpcResponse,
    ) -> String;

    /// Send a Rest request to the current blockchain.
    fn blockchain_rest(
        /// This is the url of the rest endpoint.
        url: String,
        /// These are the configuration options for parsing the response.
        response: config::RestResponse,
    ) -> String;

    /// Send a Sh request to the current blockchain.
    fn blockchain_sh(
        /// These are the arguments to the sh command that is executed for this `Method`.
        body: String,
        /// These are the configuration options for parsing the response.
        response: config::ShResponse,
    ) -> String;

    /// Get logs gathered from jobs.
    #[server_streaming]
    fn get_logs() -> String;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockchainKey {
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub content: Vec<u8>,
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait LogsCollector {
    fn send_log(log: String);
}
