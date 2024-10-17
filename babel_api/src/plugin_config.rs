use crate::engine::{self, JobConfig, JobType, PosixSignal, RestartConfig};
use eyre::ensure;
use rhai::Dynamic;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

pub const DOWNLOAD_JOB_NAME: &str = "download";
pub const DOWNLOADING_STATE_NAME: &str = "downloading";
pub const UPLOAD_JOB_NAME: &str = "upload";
pub const UPLOADING_STATE_NAME: &str = "uploading";
pub const STARTING_STATE_NAME: &str = "starting";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginConfig {
    /// List of configuration files to be rendered from template with provided params.
    pub config_files: Option<Vec<ConfigFile>>,
    /// Node init actions.
    pub init: Option<Actions>,
    /// List of protocol services.
    pub services: Vec<Service>,
    /// Download configuration.
    pub download: Option<Download>,
    /// Alternative download configuration.
    pub alternative_download: Option<AlternativeDownload>,
    /// List of post-download jobs.
    pub post_download: Option<Vec<Job>>,
    /// List of pre-upload actions.
    pub pre_upload: Option<Actions>,
    /// Upload configuration.
    pub upload: Option<Upload>,
    /// List of post-upload jobs.
    pub post_upload: Option<Vec<Job>>,
    /// List of tasks to be scheduled on init.
    pub scheduled: Option<Vec<Task>>,
    /// Set to true, to disable all default services.
    /// Default to false.
    #[serde(default)]
    pub disable_default_services: bool,
}

impl PluginConfig {
    pub fn validate(&self, default_services: Option<&Vec<DefaultService>>) -> eyre::Result<()> {
        // Scheduled tasks name uniqueness
        if let Some(scheduled) = &self.scheduled {
            let mut unique = HashSet::new();
            ensure!(
                scheduled.iter().all(|task| unique.insert(&task.name)),
                "Scheduled tasks names are not unique"
            );
        }
        // Jobs name uniqueness
        let mut unique = HashSet::new();
        unique.insert(UPLOAD_JOB_NAME);
        unique.insert(DOWNLOAD_JOB_NAME);
        if let Some(default_services) = default_services {
            ensure!(
                default_services
                    .iter()
                    .all(|service| unique.insert(&service.name)),
                "Default services names are not unique"
            );
        }
        if let Some(init) = &self.init {
            ensure!(
                init.jobs.iter().all(|job| unique.insert(&job.name)),
                "Init jobs names are not unique"
            );
        }
        if let Some(post_download) = &self.post_download {
            ensure!(
                post_download.iter().all(|job| unique.insert(&job.name)),
                "Post-download jobs names are not unique"
            );
        }
        ensure!(
            self.services
                .iter()
                .all(|service| unique.insert(&service.name)),
            "Services names are not unique"
        );
        if let Some(pre_upload) = &self.pre_upload {
            ensure!(
                pre_upload.jobs.iter().all(|job| unique.insert(&job.name)),
                "Pre-upload jobs names are not unique"
            );
        }
        if let Some(post_upload) = &self.post_upload {
            ensure!(
                post_upload.iter().all(|job| unique.insert(&job.name)),
                "Post-upload jobs names are not unique"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseConfig {
    /// List of configuration files to be rendered from template with provided params.
    pub config_files: Option<Vec<ConfigFile>>,
    /// List of default services to be run in background.
    pub services: Option<Vec<DefaultService>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigFile {
    /// It assumes that file pointed by `template` argument exists.
    pub template: PathBuf,
    /// File pointed by `destination` path will be overwritten if exists.
    pub destination: PathBuf,
    /// Map object serializable to JSON.
    /// See [Tera Docs](https://keats.github.io/tera/docs/#templates) for details on templating syntax.
    pub params: Dynamic,
}

/// Definition of default service that should be run on all nodes (if supported by image).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DefaultService {
    /// Name of service. This will be used as Job name, so make sure it is unique enough,
    /// to not collide with other jobs.
    pub name: String,
    /// Sh script body. It shall be blocking (foreground) process.
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
    /// Capacity of log buffer (in megabytes).
    pub log_buffer_capacity_mb: Option<usize>,
    /// Prepend timestamp to each log, or not.
    pub log_timestamp: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Actions {
    /// List of sh commands to be executed first.
    pub commands: Vec<String>,
    /// List of long-running tasks (aka jobs) that must be finished before protocol services start.
    pub jobs: Vec<Job>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Indicates that this job will never be restarted, whether succeeded or not - appropriate for jobs
    /// that can't be simply restarted on failure (e.g. need some manual actions).
    Never,
    /// Job is restarted only if `exit_code != 0`.
    OnFailure(RestartConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    /// Unique job name.
    pub name: String,
    /// Sh script body. It shall be blocking (foreground) process.
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
    /// Capacity of log buffer (in megabytes).
    pub log_buffer_capacity_mb: Option<usize>,
    /// Prepend timestamp to each log, or not.
    pub log_timestamp: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Service {
    /// Unique job name.
    pub name: String,
    /// Sh script body. It shall be blocking (foreground) process.
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
    /// Flag indicating if service uses protocol data.
    /// Services that uses protocol data won't be started until all init steps and download
    /// is finished. They are also stopped while uploading protocol archive.
    /// Default to true if not set.
    #[serde(default = "default_use_protocol_data")]
    pub use_protocol_data: bool,
    /// Capacity of log buffer (in megabytes).
    pub log_buffer_capacity_mb: Option<usize>,
    /// Prepend timestamp to each log, or not.
    pub log_timestamp: Option<bool>,
}
fn default_use_protocol_data() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Download {
    /// Download restart config.
    pub restart_config: Option<RestartConfig>,
    /// Maximum number of parallel opened connections.
    pub max_connections: Option<usize>,
    /// Maximum number of parallel workers.
    pub max_runners: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlternativeDownload {
    /// Sh script body. It shall be blocking (foreground) process.
    pub run_sh: String,
    /// AlternativeDownload restart config.
    pub restart_config: Option<RestartConfig>,
    /// Run job as a different user.
    pub run_as: Option<String>,
    /// Capacity of log buffer (in megabytes).
    pub log_buffer_capacity_mb: Option<usize>,
    /// Prepend timestamp to each log, or not.
    pub log_timestamp: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// Number of chunks that protocol data should be split into.
    /// Recommended chunk size is about 500MB.
    pub number_of_chunks: Option<u32>,
    /// Seconds after which presigned urls in generated `UploadManifest` may expire.
    pub url_expires_secs: Option<u32>,
    /// Version number for uploaded data. Auto-assigned if `None`.
    pub data_version: Option<u64>,
}

/// Type of compression used on chunk data.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
        wait_for: None,
        run_as: job.run_as,
        log_buffer_capacity_mb: job.log_buffer_capacity_mb,
        log_timestamp: job.log_timestamp,
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
                max_connections: download.max_connections,
                max_runners: download.max_runners,
            },
            restart: engine::RestartPolicy::OnFailure(
                download.restart_config.unwrap_or(DEFAULT_RESTART_CONFIG),
            ),
            shutdown_timeout_secs: None,
            shutdown_signal: None,
            needs: Some(init_jobs),
            wait_for: None,
            run_as: None,
            log_buffer_capacity_mb: None,
            log_timestamp: None,
        }
    } else {
        JobConfig {
            job_type: JobType::Download {
                max_connections: None,
                max_runners: None,
            },
            restart: engine::RestartPolicy::OnFailure(DEFAULT_RESTART_CONFIG),
            shutdown_timeout_secs: None,
            shutdown_signal: None,
            needs: Some(init_jobs),
            wait_for: None,
            run_as: None,
            log_buffer_capacity_mb: None,
            log_timestamp: None,
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
        wait_for: None,
        run_as: alternative_download.run_as,
        log_buffer_capacity_mb: alternative_download.log_buffer_capacity_mb,
        log_timestamp: alternative_download.log_timestamp,
    }
}

pub fn build_service_job_config(
    service: Service,
    needs: Vec<String>,
    wait_for: Vec<String>,
) -> JobConfig {
    JobConfig {
        job_type: JobType::RunSh(service.run_sh),
        restart: engine::RestartPolicy::Always(service.restart_config.unwrap_or(RestartConfig {
            backoff_timeout_ms: 60_000,
            backoff_base_ms: 1_000,
            max_retries: None,
        })),
        shutdown_timeout_secs: service.shutdown_timeout_secs,
        shutdown_signal: service.shutdown_signal,
        needs: if service.use_protocol_data {
            Some(needs)
        } else {
            None
        },
        wait_for: if service.use_protocol_data {
            Some(wait_for)
        } else {
            None
        },
        run_as: service.run_as,
        log_buffer_capacity_mb: service.log_buffer_capacity_mb,
        log_timestamp: service.log_timestamp,
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
            wait_for: None,
            run_as: None,
            log_buffer_capacity_mb: None,
            log_timestamp: None,
        }
    } else {
        JobConfig {
            job_type: JobType::Upload {
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
            wait_for: None,
            run_as: None,
            log_buffer_capacity_mb: None,
            log_timestamp: None,
        }
    }
}
