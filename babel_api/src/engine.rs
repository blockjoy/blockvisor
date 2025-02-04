use eyre::{anyhow, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::Level;
use url::Url;

pub const DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_JOB_SHUTDOWN_SIGNAL: PosixSignal = PosixSignal::SIGTERM;

/// Plugin engine must implement this interface, so it can be used by babel plugins.
pub trait Engine {
    /// Create background job with unique name.
    fn create_job(&self, job_name: &str, job_config: JobConfig) -> Result<()>;
    /// Start background job with given unique name.
    fn start_job(&self, job_name: &str) -> Result<()>;
    /// Stop background job with given unique name if running.
    fn stop_job(&self, job_name: &str) -> Result<()>;
    /// Cleanup background job with given unique name - remove any intermediate files,
    /// so next time it will start from scratch.
    fn cleanup_job(&self, job_name: &str) -> Result<()>;
    /// Get background job info by unique name.
    fn job_info(&self, job_name: &str) -> Result<JobInfo>;
    /// Get background jobs info.
    fn get_jobs(&self) -> Result<JobsInfo>;

    /// Execute Jrpc request on the current node and return its http response. See `HttpResponse`.
    fn run_jrpc(&self, req: JrpcRequest, timeout: Option<Duration>) -> Result<HttpResponse>;

    /// Execute a Rest request on the current node and return its http response. See `HttpResponse`.
    fn run_rest(&self, req: RestRequest, timeout: Option<Duration>) -> Result<HttpResponse>;

    /// Run Sh script on the current node and return its response. See `ShResponse` for details.
    fn run_sh(&self, body: &str, timeout: Option<Duration>) -> Result<ShResponse>;

    /// Allowing people to substitute arbitrary data into sh-commands is unsafe.
    /// Call this function over each value before passing it to `run_sh`. This function is deliberately more
    /// restrictive than needed; it just filters out each character that is not a number or a
    /// string or absolutely needed to form an url or json file.
    fn sanitize_sh_param(&self, param: &str) -> Result<String>;

    /// This function renders configuration template with provided `params`.
    /// See [Tera Docs](https://tera.netlify.app/docs/#templates) for details on templating syntax.
    /// `params` is expected to be JSON serialized to string.
    /// It assumes that file pointed by `template` argument exists.
    /// File pointed by `destination` path will be overwritten if exists.
    fn render_template(&self, template: &Path, destination: &Path, params: &str) -> Result<()>;

    /// Get node params as key-value map.
    fn node_params(&self) -> HashMap<String, String>;

    /// Get node env structure.
    fn node_env(&self) -> NodeEnv;

    /// Save plugin data to persistent storage.
    fn save_data(&self, value: &str) -> Result<()>;

    /// Load plugin data from persistent storage.
    fn load_data(&self) -> Result<String>;

    /// Handle logs from plugin.
    fn log(&self, level: Level, message: &str);

    /// Schedule periodic function call according to given cron like schedule.
    fn add_task(
        &self,
        task_name: &str,
        schedule: &str,
        function_name: &str,
        function_param: &str,
    ) -> Result<()>;
    /// Delete previously scheduled task.
    fn delete_task(&self, task_name: &str) -> Result<()>;

    /// Checks if protocol data has been already locked.
    fn is_protocol_data_locked(&self) -> Result<bool>;
    /// Checks if protocol archive is available.
    fn has_protocol_archive(&self) -> Result<bool>;
    /// Get plugin secret from remote encrypted storage
    fn get_secret(&self, name: &str) -> Result<Option<Vec<u8>>>;
    /// Put plugin secret to remote encrypted storage
    fn put_secret(&self, name: &str, value: Vec<u8>) -> Result<()>;
    /// Read file located on node filesystem.
    fn file_read(&self, path: &Path) -> Result<Vec<u8>>;
    /// Write file into node filesystem.
    fn file_write(&self, path: &Path, content: Vec<u8>) -> Result<()>;
}

/// Node environment/context metadata.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeEnv {
    /// Node id.
    pub node_id: String,
    /// Node name.
    pub node_name: String,
    /// Organisation id to which node belongs to.
    pub node_org_id: String,
    /// Node version.
    pub node_version: String,
    /// Node protocol.
    pub node_protocol: String,
    /// Node variant.
    pub node_variant: String,
    /// Node IP.
    pub node_ip: String,
    /// Node gateway IP.
    pub node_gateway: String,
    /// Indicate if node run in dev mode.
    pub dev_mode: bool,
    /// Host id.
    pub bv_host_id: String,
    /// Host name.
    pub bv_host_name: String,
    /// API url used by host.
    pub bv_api_url: String,
    /// Absolute path to directory where data drive is mounted.
    pub data_mount_point: PathBuf,
    /// Absolute path to directory where protocol data are stored.
    pub protocol_data_path: PathBuf,
}

/// Structure describing where decompressed data shall be written to and how many bytes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileLocation {
    /// Relative file path
    pub path: PathBuf,
    /// Position of data in the file
    pub pos: u64,
    /// Size of uncompressed data
    pub size: u64,
}

/// Checksum variant.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Checksum {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
    Blake3([u8; 32]),
}

/// Data is stored on the cloud in chunks. Each chunk may map into part
/// of a single file or multiple files (i.e. original disk representation).
/// Downloaded chunks, after decompression, shall be written into disk
/// location(s) described by the destinations.
///
/// Example of chunk-file mapping:
///```ascii flow
///                 path: file1           path: file1          path: file1
///  compressed     pos: 0                pos: 1024            pos: 2048
///  ─────────┐     size: 1024            size: 1024           size: 1024
///           │   ┌────────────────────┬────────────────────┬───────────────────┐
///           │   │  decompressed      │                    │                   │
///           │   └────────────────────┴────────────────────┴───────────────────┘
///           │                 ▲                 ▲                  ▲
///           ▼                 │                 │                  │
///         ┌──┐                │                 │                  │
/// chunk 1 │  │                │                 │                  │
///         │  ├────────────────┘                 │                  │
///         │  │                                  │                  │
///         └──┘                                  │                  │
///         ┌──┐                                  │                  │
/// chunk 2 │  │                                  │                  │
///         │  ├──────────────────────────────────┘                  │
///         │  │                                                     │
///         └──┘                                                     │
///         ┌──┐                                                     │
/// chunk 3 │  │     download and decompress                         │
///         │  ├─────────────────────────────────────────────────────┘
///         │  │
///         └──┘
///         ┌──┐         ┌──────┐ path: file2
/// chunk 4 │  ├────────►│      │ pos: 0
///         │  │         └──────┘ size: 256
///         ├──┤
///         │  │         ┌──────────────┐ path: file3
///         │  ├────────►│              │ pos: 0
///         │  │         └──────────────┘ size: 512
///         │  │
///         ├──┤         ┌────┐ path: file4
///         │  ├────────►│    │ pos: 0
///         └──┘         └────┘ size: 128
///```
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    pub index: u32,
    /// Persistent chunk key
    pub key: String,
    /// Pre-signed download url (may be temporary),
    /// May be `None` when uploading manifest blueprint
    pub url: Option<Url>,
    /// Chunk data checksum
    pub checksum: Checksum,
    /// Chunk size in bytes
    pub size: u64,
    /// Chunk to data files mapping
    pub destinations: Vec<FileLocation>,
}

/// Type of compression used on chunk data.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum Compression {
    ZSTD(i32),
}

/// Download manifest, describing a cloud to disk mapping.
/// Sometimes it is necessary to put data into the cloud in a different form,
/// because of cloud limitations or needed optimization.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadManifest {
    /// Total size of uncompressed data
    pub total_size: u64,
    /// Chunk compression type or none
    pub compression: Option<Compression>,
    /// Full list of chunks
    pub chunks: Vec<Chunk>,
}

/// Download manifest metadata.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DownloadMetadata {
    /// Total size of uncompressed data
    pub total_size: u64,
    /// Chunk compression type or none
    pub compression: Option<Compression>,
    /// Number of chunks
    pub chunks: u32,
    /// Archive version number.
    pub data_version: u64,
}

/// Slot represents destination for chunk to be uploaded.
/// This is just placeholder that MAY be used to upload chunk.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Slot {
    pub index: u32,
    /// Persistent slot/chunk key
    pub key: String,
    /// Pre-signed upload url (maybe temporary)
    pub url: Url,
}

/// Upload manifest is a list of slots, which consists
/// of pre-signed upload urls for each chunk to be uploaded.
/// This is just placeholder that MAY be used to upload data represented then by `DownloadManifest`.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct UploadSlots {
    /// Version number for uploaded data. Auto-assigned if `None`.
    pub slots: Vec<Slot>,
    /// Archive version number.
    pub data_version: u64,
}

/// Type of long-running job.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    /// Shell script - takes script body as a `String`. It shall be blocking (foreground) process.
    RunSh(String),
    /// Download data - according to given manifest.
    Download {
        /// Maximum number of parallel opened connections.
        max_connections: Option<usize>,
        /// Maximum number of parallel workers.
        max_runners: Option<usize>,
    },
    /// Upload data - according to given manifest.
    Upload {
        /// List of exclude patterns. Files in `source` directory that match any of pattern,
        /// won't be taken into account.
        exclude: Option<Vec<String>>,
        /// Compression to be used on chunks.
        compression: Option<Compression>,
        /// Maximum number of parallel opened connections.
        max_connections: Option<usize>,
        /// Maximum number of parallel workers.
        max_runners: Option<usize>,
        /// Number of chunks that protocol data should be split into.
        /// Recommended chunk size is about 500MB.
        number_of_chunks: Option<u32>,
        /// Seconds after which presigned urls in generated `UploadManifest` may expire.
        url_expires_secs: Option<u32>,
        /// Version number for uploaded data. Auto-assigned if `None`.
        data_version: Option<u64>,
    },
}

/// Jrpc request
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JrpcRequest {
    /// This is the host for the JSON rpc request.
    pub host: String,
    /// The name of the jRPC method that we are going to call into.
    pub method: String,
    /// Optional params structure in form of serialized JSON.
    /// In jPRC it could be either Array (for positional parameters), or Object (for named ones)
    pub params: Option<String>,
    /// Extra HTTP headers to be added to the request.
    pub headers: Option<Vec<(String, String)>>,
}

/// REST request
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RestRequest {
    /// This is the url of the rest endpoint.
    pub url: String,
    /// Extra HTTP headers to be added to request.
    pub headers: Option<Vec<(String, String)>>,
}

/// Long-running job configuration
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JobConfig {
    /// Job type.
    pub job_type: JobType,
    /// Job restart policy.
    pub restart: RestartPolicy,
    /// Job shutdown timeout - how long it may take to gracefully shutdown the job.
    /// After given time job won't be killed, but babel will rise the error.
    /// If not set default to 60s.
    pub shutdown_timeout_secs: Option<u64>,
    /// POSIX signal that will be sent to child processes on job shutdown.
    /// See [man7](https://man7.org/linux/man-pages/man7/signal.7.html) for possible values.
    /// If not set default to `SIGTERM`.
    pub shutdown_signal: Option<PosixSignal>,
    /// List of job names that this job needs to be finished before start.
    pub needs: Option<Vec<String>>,
    /// List of job names that this job needs to wait for, but job will be started
    /// whatever waited jobs result is.
    pub wait_for: Option<Vec<String>>,
    /// Run job as a different user.
    pub run_as: Option<String>,
    /// Capacity of log buffer (in megabytes).
    pub log_buffer_capacity_mb: Option<usize>,
    /// Prepend timestamp to each log, or not.
    pub log_timestamp: Option<bool>,
    /// Indicate if job should 'lock' protocol data.
    /// Lock prevents re-initialization of the data after job is started.
    /// Default to `false`.
    pub protocol_data_lock: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Indicates that this job will never be restarted, whether succeeded or not - appropriate for jobs
    /// that can't be simply restarted on failure (e.g. need some manual actions).
    Never,
    /// Job is always restarted - equivalent to entrypoint.
    Always(RestartConfig),
    /// Job is restarted only if `exit_code != 0`.
    OnFailure(RestartConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RestartConfig {
    /// if job stay alive given amount of time (in miliseconds) backoff is reset
    pub backoff_timeout_ms: u64,
    /// base time (in miliseconds) for backof, multiplied by consecutive power of 2 each time
    pub backoff_base_ms: u64,
    /// maximum number of retries, or `None` if there is no such limit
    pub max_retries: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// The current job was requested to start, but the process has not been launched yet.
    /// It was not picked by the JobRunner yet, or needs another job to be finished first.
    /// Every job starts with this state.
    Pending { waiting_for: Vec<String> },
    /// The JobRunner actually picked that job.
    Running,
    /// Job finished - successfully or not. It means that the JobRunner won't try to restart that job anymore.
    Finished {
        /// Job exit code, if any. For `run_sh` job type it is script exit code.
        /// `None` always means some error, usually before the process was even started (e.g. needed job failed).  
        exit_code: Option<i32>,
        /// Error description or empty if successful.
        message: String,
    },
    /// Job was explicitly stopped.
    Stopped,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Hash)]
pub struct JobInfo {
    /// Job status.
    pub status: JobStatus,
    /// Job progress
    pub progress: Option<JobProgress>,
    /// Restart count from last 24h.
    pub restart_count: usize,
    /// Job related logs from last 24h (max. 1024 entries).
    pub logs: Vec<String>,
    /// Node can't be upgraded while `upgrade_blocking` job is running.
    pub upgrade_blocking: bool,
}

pub type JobsInfo = HashMap<String, JobInfo>;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Hash)]
pub struct JobProgress {
    /// Total amount of units of work to process
    pub total: u32,
    /// Amount of currently processed units of work
    pub current: u32,
    /// Free form progress message to report to the user
    pub message: String,
}

/// Http response.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HttpResponse {
    /// Http status code.
    pub status_code: u16,
    /// Response body as text.
    pub body: String,
}

/// Sh script response.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShResponse {
    /// script exit code
    pub exit_code: i32,
    /// stdout
    pub stdout: String,
    /// stderr
    pub stderr: String,
}

impl UploadSlots {
    /// Validate manifest internal consistency.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.slots.is_empty(),
            anyhow!("corrupted manifest - expected at least one slot")
        );
        Ok(())
    }
}

/// See [man7](https://man7.org/linux/man-pages/man7/signal.7.html)
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum PosixSignal {
    /// Abort signal from abort(3)
    SIGABRT,
    /// Timer signal from alarm(2)
    SIGALRM,
    /// Bus error (bad memory access)
    SIGBUS,
    /// Child stopped or terminated
    SIGCHLD,
    /// A synonym for SIGCHLD
    SIGCLD,
    /// Continue if stopped
    SIGCONT,
    /// Emulator trap
    SIGEMT,
    /// Floating-point exception
    SIGFPE,
    /// Hangup detected on controlling terminal or death of controlling process
    SIGHUP,
    /// Illegal Instruction
    SIGILL,
    /// A synonym for SIGPWR
    SIGINFO,
    /// Interrupt from keyboard
    SIGINT,
    /// I/O now possible (4.2BSD)
    SIGIO,
    /// IOT trap. A synonym for SIGABRT
    SIGIOT,
    /// Kill signal
    SIGKILL,
    /// Broken pipe: write to pipe with no readers; see pipe(7)
    SIGPIPE,
    /// Pollable event (Sys V); synonym for SIGIO
    SIGPOLL,
    /// Profiling timer expired
    SIGPROF,
    /// Power failure (System V)
    SIGPWR,
    /// Quit from keyboard
    SIGQUIT,
    /// Invalid memory reference
    SIGSEGV,
    /// Stop process
    SIGSTOP,
    /// Stop typed at terminal
    SIGTSTP,
    /// Bad system call (SVr4); see also seccomp(2)
    SIGSYS,
    /// Termination signal
    SIGTERM,
    /// Trace/breakpoint trap
    SIGTRAP,
    /// Terminal input for background process
    SIGTTIN,
    /// Terminal output for background process
    SIGTTOU,
    /// Synonymous with SIGSYS
    SIGUNUSED,
    /// Urgent condition on socket (4.2BSD)
    SIGURG,
    /// User-defined signal 1
    SIGUSR1,
    /// User-defined signal 2
    SIGUSR2,
    /// Virtual alarm clock (4.2BSD)
    SIGVTALRM,
    /// CPU time limit exceeded (4.2BSD); see setrlimit(2)
    SIGXCPU,
    /// File size limit exceeded (4.2BSD); see setrlimit(2)
    SIGXFSZ,
    /// Window resize signal (4.3BSD, Sun)
    SIGWINCH,
}

impl fmt::Debug for DownloadManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DownloadManifest(total_size: {:?}, compression: {:?}, chunks: [{:?}, ...])",
            self.total_size,
            self.compression,
            self.chunks.first()
        )
    }
}

impl fmt::Debug for UploadSlots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UploadManifest(slots: [{:?}, ...])", self.slots.first())
    }
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobStatus::Pending { waiting_for } => {
                write!(f, "Pending (waiting for: {waiting_for:?})")
            }
            JobStatus::Running => write!(f, "Running"),
            JobStatus::Finished {
                exit_code: Some(exit_code),
                message,
            } if message.is_empty() => write!(f, "Finished with exit code {exit_code}"),
            JobStatus::Finished {
                exit_code: Some(exit_code),
                message,
            } => write!(
                f,
                "Finished with exit code {exit_code} and message `{message}`"
            ),
            JobStatus::Finished {
                exit_code: None,
                message,
            } if message.is_empty() => write!(f, "Finished"),
            JobStatus::Finished {
                exit_code: None,
                message,
            } => write!(f, "Finished with message `{message}`"),
            JobStatus::Stopped => write!(f, "Stopped"),
        }
    }
}

impl fmt::Display for JobProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            current,
            total,
            message,
        } = self;
        let progress = *current as f32 * 100.0 / *total as f32;
        write!(f, "{progress:.2}% ({current}/{total} {message})")
    }
}

impl JobConfig {
    pub fn waiting_for(&self) -> Vec<String> {
        let mut waiting_for = self.needs.clone().unwrap_or_default();
        waiting_for.append(&mut self.wait_for.clone().unwrap_or_default());
        waiting_for
    }
}
