use crate::engine;
use crate::engine::{JobConfig, JobType, PosixSignal, RestartConfig};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PluginConfig {
    /// Node init actions.
    pub init: Option<Actions>,
    /// List of blockchain services.
    pub services: Vec<Service>,
    /// Download configuration.
    pub download: Option<Download>,
    /// Alternative download configuration.
    pub alternative_download: Option<AlternativeDownload>,
    /// List of post-download actions.
    pub post_download: Option<Actions>,
    /// List of pre-upload actions.
    pub pre_upload: Option<Actions>,
    /// Upload configuration.
    pub upload: Option<Upload>,
    /// List of post-upload actions.
    pub post_upload: Option<Actions>,
    /// List of tasks to be scheduled on init.
    pub scheduled: Option<Vec<Task>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Task {
    /// Unique name of the task.
    pub name: String,
    /// Cron schedule expression.
    pub schedule: String,
    /// Function name to be executed according to schedule.
    pub function: String,
    /// Parameter to ba passed to function.
    pub param: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Actions {
    /// List of sh commands to be executed first.
    pub commands: Vec<String>,
    /// List of long-running tasks (aka jobs) that must be finished before blockchain services start.
    pub jobs: Vec<Job>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Indicates that this job will never be restarted, whether succeeded or not - appropriate for jobs
    /// that can't be simply restarted on failure (e.g. need some manual actions).
    Never,
    /// Job is restarted only if `exit_code != 0`.
    OnFailure(RestartConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Job {
    /// Unique job name.
    pub name: String,
    /// Sh script body.
    pub run_sh: String,
    /// InitJob restart policy.
    pub restart: Option<RestartPolicy>,
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
    /// Run job as a different user.
    pub run_as: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Service {
    /// Unique job name.
    pub name: String,
    /// Sh script body.
    pub run_sh: String,
    /// Service restart config.
    pub restart_config: Option<RestartConfig>,
    /// Job shutdown timeout - how long it may take to gracefully shutdown the job.
    /// After given time job won't be killed, but babel will rise the error.
    /// If not set default to 60s.
    pub shutdown_timeout_secs: Option<u64>,
    /// POSIX signal that will be sent to child processes on job shutdown.
    /// See [man7](https://man7.org/linux/man-pages/man7/signal.7.html) for possible values.
    /// If not set default to `SIGTERM`.
    pub shutdown_signal: Option<PosixSignal>,
    /// Run job as a different user.
    pub run_as: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Download {
    /// Download restart config.
    pub restart_config: Option<RestartConfig>,
    /// Maximum number of parallel opened connections.
    pub max_connections: Option<usize>,
    /// Maximum number of parallel workers.
    pub max_runners: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AlternativeDownload {
    /// Sh script body.
    pub run_sh: String,
    /// AlternativeDownload restart config.
    pub restart_config: Option<RestartConfig>,
    /// Run job as a different user.
    pub run_as: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Upload {
    /// Upload restart config.
    pub restart_config: Option<RestartConfig>,
    /// List of exclude patterns. Files in `source` directory that match any of pattern,
    /// won't be taken into account.
    pub exclude: Option<Vec<String>>,
    /// Compression to be used on chunks.
    pub compression: Option<Compression>,
    /// Maximum number of parallel opened connections.
    pub max_connections: Option<usize>,
    /// Maximum number of parallel workers.
    pub max_runners: Option<usize>,
    /// Number of chunks that blockchain data should be split into.
    /// Recommended chunk size is about 1GB.
    pub number_of_chunks: Option<u32>,
    /// Seconds after which presigned urls in generated `UploadManifest` may expire.
    pub url_expires_secs: Option<u32>,
    /// Version number for uploaded data. Auto-assigned if `None`.
    pub data_version: Option<u64>,
}

/// Type of compression used on chunk data.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum Compression {
    NONE,
    ZSTD(i32),
}

pub fn build_job_config(job: Job) -> JobConfig {
    JobConfig {
        job_type: JobType::RunSh(job.run_sh),
        restart: job
            .restart
            .map(|restart| match restart {
                RestartPolicy::Never => engine::RestartPolicy::Never,
                RestartPolicy::OnFailure(config) => engine::RestartPolicy::OnFailure(config),
            })
            .unwrap_or(engine::RestartPolicy::Never),
        shutdown_timeout_secs: job.shutdown_timeout_secs,
        shutdown_signal: job.shutdown_signal,
        needs: job.needs,
        run_as: job.run_as,
    }
}

pub fn build_download_job_config(download: Option<Download>, init_jobs: Vec<String>) -> JobConfig {
    const DEFAULT_RESTART_CONFIG: RestartConfig = RestartConfig {
        backoff_timeout_ms: 600_000,
        backoff_base_ms: 500,
        max_retries: Some(10),
    };
    if let Some(download) = download {
        JobConfig {
            job_type: JobType::Download {
                destination: None,
                max_connections: download.max_connections,
                max_runners: download.max_runners,
            },
            restart: engine::RestartPolicy::OnFailure(
                download.restart_config.unwrap_or(DEFAULT_RESTART_CONFIG),
            ),
            shutdown_timeout_secs: None,
            shutdown_signal: None,
            needs: Some(init_jobs),
            run_as: None,
        }
    } else {
        JobConfig {
            job_type: JobType::Download {
                destination: None,
                max_connections: None,
                max_runners: None,
            },
            restart: engine::RestartPolicy::OnFailure(DEFAULT_RESTART_CONFIG),
            shutdown_timeout_secs: None,
            shutdown_signal: None,
            needs: Some(init_jobs),
            run_as: None,
        }
    }
}

pub fn build_alternative_download_job_config(
    alternative_download: AlternativeDownload,
    init_jobs: Vec<String>,
) -> JobConfig {
    JobConfig {
        job_type: JobType::RunSh(alternative_download.run_sh),
        restart: if let Some(restart) = alternative_download.restart_config {
            engine::RestartPolicy::OnFailure(restart)
        } else {
            engine::RestartPolicy::Never
        },
        shutdown_timeout_secs: None,
        shutdown_signal: None,
        needs: Some(init_jobs),
        run_as: alternative_download.run_as,
    }
}

pub fn build_service_job_config(service: Service, needs: Vec<String>) -> JobConfig {
    JobConfig {
        job_type: JobType::RunSh(service.run_sh),
        restart: engine::RestartPolicy::Always(service.restart_config.unwrap_or(RestartConfig {
            backoff_timeout_ms: 60_000,
            backoff_base_ms: 1_000,
            max_retries: None,
        })),
        shutdown_timeout_secs: service.shutdown_timeout_secs,
        shutdown_signal: service.shutdown_signal,
        needs: Some(needs),
        run_as: service.run_as,
    }
}

pub fn build_upload_job_config(value: Option<Upload>, pre_upload_jobs: Vec<String>) -> JobConfig {
    const DEFAULT_RESTART_CONFIG: RestartConfig = RestartConfig {
        backoff_timeout_ms: 600_000,
        backoff_base_ms: 500,
        max_retries: Some(10),
    };
    const DEFAULT_COMPRESSION: engine::Compression = engine::Compression::ZSTD(3);
    if let Some(upload) = value {
        JobConfig {
            job_type: JobType::Upload {
                source: None,
                exclude: upload.exclude,
                compression: match upload.compression {
                    None => Some(DEFAULT_COMPRESSION),
                    Some(Compression::ZSTD(level)) => Some(engine::Compression::ZSTD(level)),
                    Some(Compression::NONE) => None,
                },
                max_connections: upload.max_connections,
                max_runners: upload.max_runners,
                number_of_chunks: upload.number_of_chunks,
                url_expires_secs: upload.url_expires_secs,
                data_version: upload.data_version,
            },
            restart: engine::RestartPolicy::OnFailure(
                upload.restart_config.unwrap_or(DEFAULT_RESTART_CONFIG),
            ),
            shutdown_timeout_secs: None,
            shutdown_signal: None,
            needs: Some(pre_upload_jobs),
            run_as: None,
        }
    } else {
        JobConfig {
            job_type: JobType::Upload {
                source: None,
                exclude: None,
                compression: Some(DEFAULT_COMPRESSION),
                max_connections: None,
                max_runners: None,
                number_of_chunks: None,
                url_expires_secs: None,
                data_version: None,
            },
            restart: engine::RestartPolicy::OnFailure(DEFAULT_RESTART_CONFIG),
            shutdown_timeout_secs: None,
            shutdown_signal: None,
            needs: Some(pre_upload_jobs),
            run_as: None,
        }
    }
}
