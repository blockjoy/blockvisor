/// This module implements job runner for for long running jobs. This includes long initialization tasks
/// and blockchain entrypoints as well, dependent on restart policy. It spawn child process as defined in
/// given config and watch it. Stopped child (whatever reason) is respawned according to given config,
/// with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset after child stays alive for at least `backoff_timeout_ms`.
use crate::{
    jobs,
    jobs::{CONFIG_SUBDIR, STATUS_SUBDIR},
    log_buffer::LogBuffer,
    utils::LimitStatus,
    utils::{kill_all_processes, Backoff},
};
use babel_api::config::{JobConfig, JobStatus, RestartConfig, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::Result;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

pub struct JobRunner<T> {
    jobs_dir: PathBuf,
    name: String,
    job_config: JobConfig,
    timer: T,
    log_buffer: LogBuffer,
}

struct JobBackoff<'a, T> {
    backoff: Option<Backoff<'a, T>>,
    max_retries: Option<u32>,
    restart_always: bool,
}

impl<'a, T: AsyncTimer> JobBackoff<'a, T> {
    fn new(timer: &'a T, mut run: RunFlag, policy: &RestartPolicy) -> Self {
        let mut build_backoff = move |cfg: &RestartConfig| {
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

    fn start(&mut self) {
        if let Some(backoff) = &mut self.backoff {
            backoff.start();
        }
    }

    /// Take proper actions when child process stopped. Depends on configured restart policy and returned
    /// exit status. `JobStatus` is returned whenever job is finished (successfully or not) - it should not
    /// be restarted anymore.
    /// `JobStatus` is returned as `Err` for convenient use with `?` operator.
    async fn stopped(&mut self, exit_code: Option<i32>, message: String) -> Result<(), JobStatus> {
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

impl<T: AsyncTimer> JobRunner<T> {
    pub fn new(timer: T, jobs_dir: &Path, name: String, log_buffer: LogBuffer) -> Result<Self> {
        let job_config = jobs::load_config(&jobs::config_file_path(
            &name,
            &jobs_dir.join(CONFIG_SUBDIR),
        ))?;

        Ok(JobRunner {
            jobs_dir: jobs_dir.to_path_buf(),
            name,
            job_config,
            timer,
            log_buffer,
        })
    }

    pub async fn run(mut self, mut run: RunFlag) {
        // Check if there are no remnant child process after previous run.
        // If so, just kill it.
        kill_all_processes("sh", &["-c", &self.job_config.body]);
        if let Err(status) = self.try_run_job(run.clone()).await {
            if let Err(err) =
                jobs::save_status(&status, &self.name, &self.jobs_dir.join(STATUS_SUBDIR))
            {
                error!("job status changed to {status:?}, but failed to save job data: {err}")
            }
            run.stop();
        }
    }

    /// Run and restart job child process until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(&mut self, mut run: RunFlag) -> Result<(), JobStatus> {
        let job_name = &self.name;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &self.job_config.body])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut backoff = JobBackoff::new(&self.timer, run.clone(), &self.job_config.restart);
        while run.load() {
            backoff.start();
            match cmd.spawn() {
                Ok(mut child) => {
                    info!("Spawned job '{job_name}'");
                    self.log_buffer
                        .attach(job_name, child.stdout.take(), child.stderr.take());
                    tokio::select!(
                        exit_status = child.wait() => {
                            let message = format!("Job '{job_name}' finished with {exit_status:?}");
                            backoff
                                .stopped(exit_status.ok().and_then(|exit| exit.code()), message)
                                .await?;
                        },
                        _ = run.wait() => {
                            info!("Job runner requested to stop, killing job '{job_name}'");
                            let _ = child.kill().await;
                        },
                    );
                }
                Err(err) => {
                    backoff
                        .stopped(None, format!("Failed to spawn job '{job_name}': {err}"))
                        .await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use babel_api::config::JobConfig;
    use bv_utils::timer::MockAsyncTimer;
    use std::fs;
    use std::{io::Write, os::unix::fs::OpenOptionsExt};

    #[tokio::test]
    async fn test_stopped_restart_never() -> Result<()> {
        let test_run = RunFlag::default();
        let timer_mock = MockAsyncTimer::new();
        let mut backoff = JobBackoff::new(&timer_mock, test_run, &RestartPolicy::Never);
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
            &timer_mock,
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
            &timer_mock,
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

    #[tokio::test]
    async fn test_run_with_logs() -> Result<()> {
        let job_name = "job_name".to_string();
        let tmp_root = TempDir::new()?.to_path_buf();
        let jobs_dir = tmp_root.join("jobs");
        fs::create_dir_all(&jobs_dir.join(CONFIG_SUBDIR))?;
        fs::create_dir_all(&jobs_dir.join(STATUS_SUBDIR))?;
        let test_run = RunFlag::default();
        let log_buffer = LogBuffer::new(16);
        let mut log_rx = log_buffer.subscribe();
        let cmd_path = tmp_root.join("test_cmd");
        {
            let mut cmd_file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o770)
                .open(&cmd_path)?;
            writeln!(cmd_file, "#!/bin/sh")?;
            writeln!(cmd_file, "echo 'cmd log'")?;
        }
        let config = JobConfig {
            body: cmd_path.to_string_lossy().to_string(),
            restart: RestartPolicy::Always(RestartConfig {
                backoff_timeout_ms: 1000,
                backoff_base_ms: 100,
                max_retries: Some(3),
            }),
            needs: vec![],
        };
        jobs::save_config(&config, &job_name, &jobs_dir.join(CONFIG_SUBDIR))?;

        let mut timer_mock = MockAsyncTimer::new();
        let now = std::time::Instant::now();
        timer_mock.expect_now().returning(move || now);
        timer_mock.expect_sleep().returning(|_| ());
        JobRunner::new(timer_mock, &jobs_dir, job_name.clone(), log_buffer)?
            .run(test_run)
            .await;

        let status = jobs::load_status(&jobs::status_file_path(
            &job_name,
            &jobs_dir.join(STATUS_SUBDIR),
        ))?;
        assert_eq!(
            status,
            JobStatus::Finished {
                exit_code: Some(0),
                message: "Job 'job_name' finished with Ok(ExitStatus(unix_wait_status(0)))"
                    .to_string()
            }
        );
        assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // first start
        assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // retry 1
        assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // retry 2
        assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // retry 3
        log_rx.try_recv().unwrap_err();
        Ok(())
    }
}
