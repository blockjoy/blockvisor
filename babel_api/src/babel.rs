use crate::utils::BabelConfig;
use crate::{
    engine::{
        Chunk, DownloadManifest, DownloadMetadata, HttpResponse, JobConfig, JobInfo, JobsInfo,
        JrpcRequest, RestRequest, ShResponse, UploadSlots,
    },
    utils::{Binary, BinaryStatus},
};
use std::{path::PathBuf, time::Duration};

#[tonic_rpc::tonic_rpc(bincode)]
pub trait Babel {
    /// Get installed version of babel.
    fn get_version() -> String;
    /// Initial Babel setup that must be run on node startup Mount data directory.
    fn setup_babel(config: BabelConfig);
    /// Get maximum time it may take to gracefully shutdown babel with all running jobs.
    fn get_babel_shutdown_timeout() -> Duration;
    /// Try gracefully shutdown babel before node stop/restart. In particular, it gracefully shut down all jobs.
    /// All `Running` jobs will be shutdown and won't start again until node is started again.
    fn shutdown_babel(force: bool);
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
    /// Skip background job with given unique name if running.
    fn skip_job(job_name: String);
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
    /// It assumes that file pointed by `template` argument exists.
    /// File pointed by `destination` path will be overwritten if exists.
    fn render_template(
        /// Path to template file.
        template: PathBuf,
        /// Path to rendered config file.
        destination: PathBuf,
        /// Parameters to be applied on template in form of serialized JSON.
        params: String,
    );

    /// Write binary content into file inside node.
    #[client_streaming]
    fn file_write(content: Binary);
    /// Read binary file from inside node.
    #[server_streaming]
    fn file_read(path: PathBuf) -> Binary;

    /// Checks if blockchain data has been already downloaded.
    fn is_download_completed() -> bool;
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait JobsMonitor {
    fn push_log(name: String, log: String);
    fn register_restart(name: String);
}

#[tonic_rpc::tonic_rpc(bincode)]
pub trait BabelEngine {
    /// Send `DownloadManifest` blueprint to API.
    fn put_download_manifest(manifest: DownloadManifest, data_version: u64);
    /// Get `DownloadMetadata` from API.
    fn get_download_metadata() -> DownloadMetadata;
    /// Get `Chunk`s to download from API.
    fn get_download_chunks(data_version: u64, chunk_indexes: Vec<u32>) -> Vec<Chunk>;
    /// Get upload `Slot`s from API.
    fn get_upload_slots(
        data_version: Option<u64>,
        slots: Vec<u32>,
        url_expires_secs: u32,
    ) -> UploadSlots;
    /// Notify about finished job, so upgrade can be retried.
    fn upgrade_blocking_jobs_finished();
    /// Sent error message to blockvisord so alert can be triggered.
    fn bv_error(message: String);
}
