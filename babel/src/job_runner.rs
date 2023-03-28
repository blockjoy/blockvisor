use crate::log_buffer::LogBuffer;
use crate::utils::LimitStatus;
/// This module implements job runner for for long running jobs. This includes long initialization tasks
/// and blockchain entrypoints as well, dependent on restart policy. It spawn child process as defined in
/// given config and watch it. Stopped child (whatever reason) is respawned according to given config,
/// with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset after child stays alive for at least `backoff_timeout_ms`.
use crate::{
    job_data::JobData,
    utils::{kill_remnants, Backoff},
};
use babel_api::config::{JobStatus, RestartConfig, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use sysinfo::{System, SystemExt};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

pub struct JobRunner<T> {
    jobs_dir: PathBuf,
    job_data: JobData,
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
        let job_data = JobData::load(&JobData::file_path(&name, jobs_dir)).with_context(|| {
            format!(
                "failed to load job '{}' data from {}",
                name,
                jobs_dir.display()
            )
        })?;

        Ok(JobRunner {
            jobs_dir: jobs_dir.to_path_buf(),
            job_data,
            timer,
            log_buffer,
        })
    }

    pub async fn run(mut self, mut run: RunFlag) {
        self.kill_all_remnants();
        if let Err(status) = self.try_run_job(run.clone()).await {
            self.job_data.status = status;
            if let Err(err) = self.job_data.save(&self.jobs_dir) {
                error!("failed to save job data: {err}")
            }
            run.stop();
        }
    }

    /// Run and restart job child process until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(&mut self, mut run: RunFlag) -> Result<(), JobStatus> {
        self.job_data.status = JobStatus::Running(None);
        let job_name = &self.job_data.name;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &self.job_data.config.body])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut backoff = JobBackoff::new(&self.timer, run.clone(), &self.job_data.config.restart);
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

    /// Check if there are no remnant child process after previous run.
    /// If so, just kill it.
    fn kill_all_remnants(&self) {
        let mut sys = System::new();
        sys.refresh_processes();
        let ps = sys.processes();
        kill_remnants("sh", &["-c", &self.job_data.config.body], ps);
    }
}

#[cfg(test)]
mod tests {}
