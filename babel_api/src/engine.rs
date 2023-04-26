use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Plugin engin must implement this interface, so it can be used by babel plugins.
pub trait Engine {
    /// Start background job with unique name.
    fn start_job(&self, job_name: &str, job_config: JobConfig) -> Result<()>;
    /// Stop background job with given unique name if running.
    fn stop_job(&self, job_name: &str) -> Result<()>;
    /// Get background job status by unique name.
    fn job_status(&self, job_name: &str) -> Result<JobStatus>;

    /// Execute Jrpc request to the current blockchain and return its response as json string.
    fn run_jrpc(&self, host: &str, method: &str) -> Result<String>;

    /// Execute a Rest request to the current blockchain and return its response as json string.
    fn run_rest(&self, url: &str) -> Result<String>;

    /// Run Sh script on the blockchain VM and return its stdout as string.
    fn run_sh(&self, body: &str) -> Result<String>;

    /// Allowing people to substitute arbitrary data into sh-commands is unsafe.
    /// Call this function over each value before passing it to `run_sh`. This function is deliberately more
    /// restrictive than needed; it just filters out each character that is not a number or a
    /// string or absolutely needed to form an url or json file.
    fn sanitize_sh_param(&self, param: &str) -> Result<String>;

    /// This function renders configuration template with provided `params`.
    /// It assumes that file pointed by `template` argument exists.
    /// File pointed by `output` path will be overwritten if exists.
    fn render_template(
        &self,
        template: &Path,
        output: &Path,
        params: HashMap<String, String>,
    ) -> Result<()>;

    /// Get node params as key-value map.
    fn node_params(&self) -> HashMap<String, String>;

    /// Save plugin data to persistent storage.
    fn save_data(&self, value: &str) -> Result<()>;

    /// Load plugin data from persistent storage.
    fn load_data(&self) -> Result<String>;
}

/// Long running job configuration
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct JobConfig {
    /// Sh script body.
    pub body: String,
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
#[serde(rename_all = "snake_case")]
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
        /// Job `sh` script exit code, if any. `None` always means some error, usually before the process
        /// was even started (e.g. needed job failed).  
        exit_code: Option<i32>,
        /// Error description or empty if successful.
        message: String,
    },
    /// Job was explicitly stopped.
    Stopped,
}
