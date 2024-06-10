use crate::{
    chroot_platform, jobs,
    pal::BabelEngineConnector,
    utils::{Backoff, LimitStatus},
    JOBS_MONITOR_UDS_PATH,
};
use async_trait::async_trait;
use babel_api::{
    babel::jobs_monitor_client::JobsMonitorClient,
    engine::{Compression, JobStatus, RestartConfig, RestartPolicy},
};
use bv_utils::{rpc::RPC_REQUEST_TIMEOUT, run_flag::RunFlag, timer::AsyncTimer, with_retry};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

const MAX_OPENED_FILES: u64 = 1024;
const MAX_BUFFER_SIZE: usize = 128 * 1024 * 1024;
const MAX_RETRIES: u32 = 5;
const BACKOFF_BASE_MS: u64 = 500;

pub type ConnectionPool = Arc<Semaphore>;

#[async_trait]
pub trait JobRunnerImpl {
    async fn try_run_job(self, run: RunFlag, name: &str) -> Result<(), JobStatus>;
}

#[async_trait]
pub trait JobRunner {
    async fn run(self, run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus;
}

#[async_trait]
pub trait Runner {
    async fn run(&mut self, run: RunFlag) -> eyre::Result<()>;
}

#[async_trait]
impl<T: JobRunnerImpl + Send> JobRunner for T {
    async fn run(self, mut run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus {
        if let Err(status) = self.try_run_job(run.clone(), name).await {
            save_job_status(&status, name, jobs_dir).await;
            run.stop();
            status
        } else {
            JobStatus::Running
        }
    }
}

pub async fn save_job_status(status: &JobStatus, name: &str, jobs_dir: &Path) {
    if let Err(err) = jobs::save_status(status, name, &jobs_dir.join(jobs::STATUS_SUBDIR)) {
        let err_msg =
            format!("job status changed to {status:?}, but failed to save job data: {err:#}");
        error!(err_msg);
        let mut client = chroot_platform::UdsConnector.connect();
        let _ = with_retry!(client.bv_error(err_msg.clone()));
    }
}

pub struct ArchiveJobRunner<T, X> {
    pub timer: T,
    pub restart_policy: RestartPolicy,
    pub runner: X,
}

impl<T: AsyncTimer + Send, X: Runner + Send> ArchiveJobRunner<T, X> {
    pub fn new(timer: T, restart_policy: RestartPolicy, xloader: X) -> Self {
        Self {
            timer,
            restart_policy,
            runner: xloader,
        }
    }

    pub async fn run(self, run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus {
        <Self as JobRunner>::run(self, run, name, jobs_dir).await
    }
}

#[async_trait]
impl<T: AsyncTimer + Send, X: Runner + Send> JobRunnerImpl for ArchiveJobRunner<T, X> {
    async fn try_run_job(mut self, mut run: RunFlag, name: &str) -> Result<(), JobStatus> {
        info!("job '{name}' started");

        let mut backoff = JobBackoff::new(name, self.timer, run.clone(), &self.restart_policy);
        while run.load() {
            backoff.start();
            match self.runner.run(run.clone()).await {
                Ok(_) => {
                    let message = format!("job '{name}' finished");
                    backoff.stopped(Some(0), message).await?;
                }
                Err(err) => {
                    backoff
                        .stopped(Some(-1), format!("job '{name}' failed with: {err:#}"))
                        .await?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct TransferConfig {
    pub max_opened_files: usize,
    pub max_runners: usize,
    pub max_connections: usize,
    pub max_buffer_size: usize,
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub archive_jobs_meta_dir: PathBuf,
    pub progress_file_path: PathBuf,
    pub compression: Option<Compression>,
}

impl TransferConfig {
    pub fn new(
        archive_jobs_meta_dir: PathBuf,
        progress_file_path: PathBuf,
        compression: Option<Compression>,
        max_connections: usize,
        max_runners: usize,
    ) -> eyre::Result<Self> {
        let max_opened_files = usize::try_from(rlimit::increase_nofile_limit(MAX_OPENED_FILES)?)?;
        Ok(Self {
            max_opened_files,
            max_runners,
            max_connections,
            max_buffer_size: MAX_BUFFER_SIZE,
            max_retries: MAX_RETRIES,
            backoff_base_ms: BACKOFF_BASE_MS,
            archive_jobs_meta_dir,
            progress_file_path,
            compression,
        })
    }
}

pub struct JobBackoff<T> {
    job_name: String,
    backoff: Option<Backoff<T>>,
    max_retries: Option<u32>,
    restart_always: bool,
}

impl<T: AsyncTimer> JobBackoff<T> {
    pub fn new(job_name: &str, timer: T, mut run: RunFlag, policy: &RestartPolicy) -> Self {
        let job_name = job_name.to_owned();
        let build_backoff = move |cfg: &RestartConfig| {
            Some(Backoff::new(
                timer,
                run.clone(),
                cfg.backoff_base_ms,
                Duration::from_millis(cfg.backoff_timeout_ms),
            ))
        };
        match policy {
            RestartPolicy::Never => Self {
                job_name,
                backoff: None,
                max_retries: None,
                restart_always: false,
            },
            RestartPolicy::Always(cfg) => Self {
                job_name,
                backoff: build_backoff(cfg),
                max_retries: cfg.max_retries,
                restart_always: true,
            },
            RestartPolicy::OnFailure(cfg) => Self {
                job_name,
                backoff: build_backoff(cfg),
                max_retries: cfg.max_retries,
                restart_always: false,
            },
        }
    }

    pub fn start(&mut self) {
        if let Some(backoff) = &mut self.backoff {
            backoff.start();
        }
    }

    /// Take proper actions when job is stopped. Depends on configured restart policy and returned
    /// exit status. `JobStatus` is returned whenever job is finished (successfully or not) - it should not
    /// be restarted anymore.
    /// `JobStatus` is returned as `Err` for convenient use with `?` operator.
    pub async fn stopped(
        &mut self,
        exit_code: Option<i32>,
        message: String,
    ) -> Result<(), JobStatus> {
        if self.restart_always || exit_code.unwrap_or(-1) != 0 {
            let job_failed = || {
                warn!(message);
                JobStatus::Finished {
                    exit_code,
                    message: message.clone(),
                }
            };
            let mut client = JobsMonitorClient::with_interceptor(
                bv_utils::rpc::build_socket_channel(JOBS_MONITOR_UDS_PATH),
                bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
            );
            let _ = with_retry!(client.push_log((self.job_name.clone(), message.clone())));
            if let Some(backoff) = &mut self.backoff {
                if let Some(max_retries) = self.max_retries {
                    if backoff.wait_with_limit(max_retries).await == LimitStatus::Exceeded {
                        Err(job_failed())
                    } else {
                        debug!("{message}  - retry");
                        let _ = with_retry!(client.register_restart(self.job_name.clone()));
                        Ok(())
                    }
                } else {
                    backoff.wait().await;
                    debug!("{message}  - retry");
                    let _ = with_retry!(client.register_restart(self.job_name.clone()));
                    Ok(())
                }
            } else {
                Err(job_failed())
            }
        } else {
            info!(message);
            Err(JobStatus::Finished {
                exit_code,
                message: "".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use babel_api::engine::RestartConfig;
    use bv_utils::timer::MockAsyncTimer;
    use eyre::Result;

    #[tokio::test]
    async fn test_stopped_restart_never() -> Result<()> {
        let test_run = RunFlag::default();
        let timer_mock = MockAsyncTimer::new();
        let mut backoff = JobBackoff::new("job_name", timer_mock, test_run, &RestartPolicy::Never);
        backoff.start(); // should do nothing
        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: "test message".to_string()
            },
            backoff
                .stopped(None, "test message".to_owned())
                .await
                .unwrap_err()
        );
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string()
            },
            backoff
                .stopped(Some(0), "test message".to_owned())
                .await
                .unwrap_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_stopped_restart_always() -> Result<()> {
        let test_run = RunFlag::default();
        let mut timer_mock = MockAsyncTimer::new();
        let now = std::time::Instant::now();
        timer_mock.expect_now().returning(move || now);
        timer_mock.expect_sleep().returning(|_| ());

        let mut backoff = JobBackoff::new(
            "job_name",
            timer_mock,
            test_run,
            &RestartPolicy::Always(RestartConfig {
                backoff_timeout_ms: 1000,
                backoff_base_ms: 100,
                max_retries: Some(1),
            }),
        );
        backoff.start();
        backoff
            .stopped(Some(0), "test message".to_owned())
            .await
            .unwrap();
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(1),
                message: "test message".to_string()
            },
            backoff
                .stopped(Some(1), "test message".to_owned())
                .await
                .unwrap_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_stopped_restart_on_failure() -> Result<()> {
        let test_run = RunFlag::default();
        let mut timer_mock = MockAsyncTimer::new();
        let now = std::time::Instant::now();
        timer_mock.expect_now().returning(move || now);
        timer_mock.expect_sleep().returning(|_| ());

        let mut backoff = JobBackoff::new(
            "job_name",
            timer_mock,
            test_run,
            &RestartPolicy::OnFailure(RestartConfig {
                backoff_timeout_ms: 1000,
                backoff_base_ms: 100,
                max_retries: Some(1),
            }),
        );
        backoff.start();
        backoff
            .stopped(Some(1), "test message".to_owned())
            .await
            .unwrap();
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(1),
                message: "test message".to_string()
            },
            backoff
                .stopped(Some(1), "test message".to_owned())
                .await
                .unwrap_err()
        );
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string()
            },
            backoff
                .stopped(Some(0), "test message".to_owned())
                .await
                .unwrap_err()
        );
        Ok(())
    }
}
