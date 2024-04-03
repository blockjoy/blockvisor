use crate::{
    engine::{
        DownloadManifest, HttpResponse, JobConfig, JobInfo, JobsInfo, JrpcRequest, RestRequest,
        ShResponse,
    },
    metadata::{firewall, BabelConfig},
    utils::{Binary, BinaryStatus},
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

#[tonic_rpc::tonic_rpc(bincode)]
pub trait Babel {
    /// Get installed version of babel.
    fn get_version() -> String;
    /// Initial Babel setup that must be run on node startup Mount data directory.
    fn setup_babel(context: NodeContext, config: BabelConfig);
    /// Get maximum time it may take to gracefully shutdown babel with all running jobs.
    fn get_babel_shutdown_timeout() -> Duration;
    /// Try gracefully shutdown babel before node stop/restart. In particular, it gracefully shut down all jobs.
    /// All `Running` jobs will be shutdown and won't start again until node is started again.
    fn shutdown_babel(force: bool);
    /// Setup firewall according to given configuration.
    fn setup_firewall(config: firewall::Config);
    /// Check if JobRunner binary exists and its checksum match expected.
    fn check_job_runner(checksum: u32) -> BinaryStatus;
    /// Upload JobRunner binary, overriding existing one (if any).
    #[client_streaming]
    fn upload_job_runner(babel_bin: Binary);
    /// Create background job with unique name. Created job is initialized with `Stopped` state.
    /// Use `start_job` to when it is time to start.
    fn create_job(job_name: String, job: JobConfig);
    /// Start background job with unique name.
    fn start_job(job_name: String);
    /// Stop background job with given unique name if running.
    fn stop_job(job_name: String);
    /// Cleanup background job with given unique name - remove any intermediate files,
    /// so next time it will start from scratch.
    fn cleanup_job(job_name: String);
    /// Get background job info by unique name.
    fn job_info(job_name: String) -> JobInfo;
    /// Get maximum time it may take to gracefully shutdown job.
    fn get_job_shutdown_timeout(job_name: String) -> Duration;
    /// Get jobs list.
    fn get_jobs() -> JobsInfo;

    /// Send a Jrpc request to the current blockchain.
    fn run_jrpc(req: JrpcRequest) -> HttpResponse;

    /// Send a Rest request to the current blockchain.
    fn run_rest(req: RestRequest) -> HttpResponse;

    /// Send a Sh request to the current blockchain.
    fn run_sh(
        /// These are the arguments to the sh command that is executed for this `Method`.
        body: String,
    ) -> ShResponse;

    /// This function renders configuration template with provided `params`.
    /// It assume that file pointed by `template` argument exists.
    /// File pointed by `output` path will be overwritten if exists.
    fn render_template(
        /// Path to template file.
        template: PathBuf,
        /// Path to rendered config file.
        output: PathBuf,
        /// Parameters to be applied on template in form of serialized JSON.
        params: String,
    );

    /// Estimate recommended number of chunks for given blockchain data.
    fn recommended_number_of_chunks(
        /// Source directory with files to be uploaded.
        source: PathBuf,
        /// List of exclude patterns. Files in `source` directory that match any of pattern,
        /// won't be taken into account.
        exclude: Option<Vec<String>>,
    ) -> u32;

    /// Get logs gathered from jobs.
    #[server_streaming]
    fn get_logs() -> String;

    /// Get logs gathered from babel processes (babel, babelsup and job_runner).
    #[server_streaming]
    fn get_babel_logs(max_lines: u32) -> String;
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait LogsCollector {
    fn send_log(log: String);
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait JobsMonitor {
    fn push_log(name: String, log: String);
    fn register_restart(name: String);
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait BabelEngine {
    /// Send `DownloadManifest` blueprint to API.
    fn put_download_manifest(manifest: DownloadManifest);
    /// Notify about finished job, so upgrade can be retried.
    fn upgrade_blocking_jobs_finished();
    /// Sent error message to blockvisord so alert can be triggered.
    fn bv_error(message: String);
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeContext {
    /// Unique id of the node.
    pub node_id: String,
    /// Friendly name of the name.
    pub node_name: String,
    /// Version of the image, that node was created from.
    pub node_version: String,
    /// Name of the blockchain protocol.
    pub protocol: String,
    /// Type of the node (validator, node, etc).
    pub node_type: String,
    /// Node IP address.
    pub ip: String,
    // Node gateway address.
    pub gateway: String,
    /// If node run in standalone mode.
    pub standalone: bool,
    /// BV host unique id.
    pub bv_id: String,
    /// BV host friendly name.
    pub bv_name: String,
    /// API url used by BV.
    pub bv_api_url: String,
    /// Unique id of the organisation associated with node.
    pub org_id: String,
}
