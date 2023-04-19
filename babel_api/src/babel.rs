use crate::engine::{JobConfig, JobStatus};
use crate::metadata::{firewall, BabelConfig, KeysConfig};
use crate::utils::{Binary, BinaryStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[tonic_rpc::tonic_rpc(bincode)]
pub trait Babel {
    /// Initial Babel setup that must be run on node startup Mount data directory.
    fn setup_babel(hostname: String, config: BabelConfig);
    /// Setup firewall according to given configuration.
    fn setup_firewall(config: firewall::Config);
    /// Download key files from locations specified in `keys` section of Babel config.
    fn download_keys(config: KeysConfig) -> Vec<BlockchainKey>;
    /// Upload files into locations specified in `keys` section of Babel config.
    fn upload_keys(config: KeysConfig, keys: Vec<BlockchainKey>) -> String;
    /// Check if JobRunner binary exists and its checksum match expected.
    fn check_job_runner(checksum: u32) -> BinaryStatus;
    /// Upload JobRunner binary, overriding existing one (if any).
    #[client_streaming]
    fn upload_job_runner(babel_bin: Binary);
    /// Start background job with unique name.
    fn start_job(job_name: String, job: JobConfig);
    /// Stop background job with given unique name if running.
    fn stop_job(job_name: String);
    /// Get background job status by unique name.
    fn job_status(job_name: String) -> JobStatus;

    /// Send a Jrpc request to the current blockchain.
    fn run_jrpc(
        /// This is the host for the JSON rpc request.
        host: String,
        /// The name of the jRPC method that we are going to call into.
        method: String,
    ) -> String;

    /// Send a Rest request to the current blockchain.
    fn run_rest(
        /// This is the url of the rest endpoint.
        url: String,
    ) -> String;

    /// Send a Sh request to the current blockchain.
    fn run_sh(
        /// These are the arguments to the sh command that is executed for this `Method`.
        body: String,
    ) -> String;

    /// This function renders configuration template with provided `params`.
    /// It assume that file pointed by `template` argument exists.
    /// File pointed by `output` path will be overwritten if exists.
    fn render_template(
        /// Path to template file.
        template: PathBuf,
        /// Path to rendered config file.
        output: PathBuf,
        /// Parameters to be applied on template.
        params: HashMap<String, String>,
    );

    /// Get logs gathered from jobs.
    #[server_streaming]
    fn get_logs() -> String;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct BlockchainKey {
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub content: Vec<u8>,
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait LogsCollector {
    fn send_log(log: String);
}
