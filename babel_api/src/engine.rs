use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::log::Level;

/// Plugin engin must implement this interface, so it can be used by babel plugins.
pub trait Engine {
    /// Start background job with unique name.
    fn start_job(&self, job_name: &str, job_config: JobConfig) -> Result<()>;
    /// Stop background job with given unique name if running.
    fn stop_job(&self, job_name: &str) -> Result<()>;
    /// Get background job status by unique name.
    fn job_status(&self, job_name: &str) -> Result<JobStatus>;

    /// Execute Jrpc request to the current blockchain and return its http response. See `HttpResponse`.
    fn run_jrpc(&self, req: JrpcRequest, timeout: Option<Duration>) -> Result<HttpResponse>;

    /// Execute a Rest request to the current blockchain and return its http response. See `HttpResponse`.
    fn run_rest(&self, req: RestRequest, timeout: Option<Duration>) -> Result<HttpResponse>;

    /// Run Sh script on the blockchain VM and return its response. See `ShResponse` for details.
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
    /// File pointed by `output` path will be overwritten if exists.
    fn render_template(&self, template: &Path, output: &Path, params: &str) -> Result<()>;

    /// Get node params as key-value map.
    fn node_params(&self) -> HashMap<String, String>;

    /// Save plugin data to persistent storage.
    fn save_data(&self, value: &str) -> Result<()>;

    /// Load plugin data from persistent storage.
    fn load_data(&self) -> Result<String>;

    /// Handle logs from plugin.
    fn log(&self, level: Level, message: &str);
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
    /// Persistent chunk key
    pub key: String,
    /// Pre-signed download url (may be temporary),
    /// May be empty when uploading manifest blueprint
    pub url: String,
    /// Chunk data checksum
    pub checksum: Checksum,
    /// Chunk size in bytes
    pub size: u64,
    /// Chunk to data files mapping
    pub destinations: Vec<FileLocation>,
}

/// Type of compression used on chunk data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    // no compression is supported yet
}

/// Download manifest, describing a cloud to disk mapping.
/// Sometimes it is necessary to put data into the cloud in a different form,
/// because of cloud limitations or needed optimization.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DownloadManifest {
    /// Total size of uncompressed data
    pub total_size: u64,
    /// Chunk compression type or none
    pub compression: Option<Compression>,
    /// Full list of chunks
    pub chunks: Vec<Chunk>,
}

/// Slot represents destination for chunk to be uploaded.
/// This is just placeholder that MAY be used to upload chunk.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Slot {
    /// Persistent slot/chunk key
    pub key: String,
    /// Pre-signed upload url (may be temporary)
    pub url: String,
}

/// Upload manifest is a list of slots, which consists
/// of pre-signed upload urls for each chunk to be uploaded.
/// And extra slot for manifest file.
/// This is just placeholder that MAY be used to upload data represented then by `DownloadManifest`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UploadManifest {
    pub slots: Vec<Slot>,
    pub manifest_slot: Slot,
}

/// Type of long running job.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    /// Shell script - takes script body as a `String`.
    RunSh(String),
    /// Download data - according to given manifest.
    Download {
        /// Manifest to be used to download data.
        /// If `None` BV will ask blockvisor-api for manifest
        /// based on node `NodeImage` and `network`.  
        manifest: Option<DownloadManifest>,
        /// Destination directory for downloaded files.
        destination: PathBuf,
    },
    /// Upload data - according to given manifest.
    Upload {
        /// Manifest to be used to upload data.
        /// If `None` BV will ask blockvisor-api for manifest
        /// based on node `NodeImage`, `network`, and size of data
        /// stored in `source` directory.  
        manifest: Option<UploadManifest>,
        /// Source directory with files to be uploaded.
        source: PathBuf,
    },
}

/// Jrpc request
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JrpcRequest {
    /// This is the host for the JSON rpc request.
    pub host: String,
    /// The name of the jRPC method that we are going to call into.
    pub method: String,
    /// Extra HTTP headers to be added to the request.
    pub headers: Option<HashMap<String, String>>,
}

/// REST request
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RestRequest {
    /// This is the url of the rest endpoint.
    pub url: String,
    /// Extra HTTP headers to be added to request.
    pub headers: Option<HashMap<String, String>>,
}

/// Long running job configuration
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JobConfig {
    /// Sh script body.
    pub job_type: JobType,
    /// Job restart policy.
    pub restart: RestartPolicy,
    /// List of job names that this job needs to be finished before start.
    pub needs: Option<Vec<String>>,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// The current job was requested to start, but the process has not been launched yet.
    /// It was not picked by the JobRunner yet, or needs another job to be finished first.
    /// Every job starts with this state.
    Pending,
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

impl DownloadManifest {
    /// Validate manifest internal consistency.
    pub fn validate(&self) -> Result<()> {
        for chunk in &self.chunks {
            if chunk.destinations.is_empty() {
                bail!("corrupted manifest - expected at least one destination file in chunk");
            }
        }
        Ok(())
    }
}

impl UploadManifest {
    /// Validate manifest internal consistency.
    pub fn validate(&self) -> Result<()> {
        if self.slots.is_empty() {
            bail!("corrupted manifest - expected at least one slot");
        }
        Ok(())
    }
}
