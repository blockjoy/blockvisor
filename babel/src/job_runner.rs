use crate::{
    jobs,
    utils::{Backoff, LimitStatus},
};
use async_trait::async_trait;
use babel_api::engine::{JobStatus, RestartConfig, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use std::{path::Path, time::Duration};
use tracing::{debug, error, info, warn};

#[async_trait]
pub trait JobRunnerImpl {
    async fn try_run_job(self, mut run: RunFlag, name: &str) -> Result<(), JobStatus>;
}

#[async_trait]
pub trait JobRunner {
    async fn run(self, mut run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus;
}

#[async_trait]
impl<T: JobRunnerImpl + Send> JobRunner for T {
    async fn run(self, mut run: RunFlag, name: &str, jobs_dir: &Path) -> JobStatus {
        if let Err(status) = self.try_run_job(run.clone(), name).await {
            if let Err(err) = jobs::save_status(&status, name, &jobs_dir.join(jobs::STATUS_SUBDIR))
            {
                error!("job status changed to {status:?}, but failed to save job data: {err}")
            }
            run.stop();
            status
        } else {
            JobStatus::Running
        }
    }
}

pub struct JobBackoff<T> {
    backoff: Option<Backoff<T>>,
    max_retries: Option<u32>,
    restart_always: bool,
}

impl<T: AsyncTimer> JobBackoff<T> {
    pub fn new(timer: T, mut run: RunFlag, policy: &RestartPolicy) -> Self {
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
                backoff: None,
                max_retries: None,
                restart_always: false,
            },
            RestartPolicy::Always(cfg) => Self {
                backoff: build_backoff(cfg),
                max_retries: cfg.max_retries,
                restart_always: true,
            },
            RestartPolicy::OnFailure(cfg) => Self {
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
            if let Some(backoff) = &mut self.backoff {
                if let Some(max_retries) = self.max_retries {
                    if backoff.wait_with_limit(max_retries).await == LimitStatus::Exceeded {
                        Err(job_failed())
                    } else {
                        debug!("{message}  - retry");
                        Ok(())
                    }
                } else {
                    backoff.wait().await;
                    debug!("{message}  - retry");
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
        let mut backoff = JobBackoff::new(timer_mock, test_run, &RestartPolicy::Never);
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
