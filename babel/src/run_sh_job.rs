/// This module implements job runner for for long running sh scripts. This includes long initialization tasks
/// and blockchain entrypoints as well, dependent on restart policy. It spawn child process as defined in
/// given config and watch it. Stopped child (whatever reason) is respawned according to given config,
/// with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset after child stays alive for at least `backoff_timeout_ms`.
use crate::{
    job_runner::{JobBackoff, JobRunner, JobRunnerImpl},
    log_buffer::LogBuffer,
    utils,
};
use async_trait::async_trait;
use babel_api::engine::{JobStatus, PosixSignal, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::Result;
use std::time::Duration;
use std::{path::Path, process::Stdio};
use tokio::process::Command;
use tracing::info;

pub struct RunShJob<T> {
    sh_body: String,
    restart_policy: RestartPolicy,
    shutdown_timeout: Duration,
    shutdown_signal: PosixSignal,
    timer: T,
    log_buffer: LogBuffer,
}

impl<T: AsyncTimer + Send> RunShJob<T> {
    pub fn new(
        timer: T,
        sh_body: String,
        restart_policy: RestartPolicy,
        shutdown_timeout: Duration,
        shutdown_signal: PosixSignal,
        log_buffer: LogBuffer,
    ) -> Result<Self> {
        Ok(Self {
            sh_body,
            restart_policy,
            shutdown_timeout,
            shutdown_signal,
            timer,
            log_buffer,
        })
    }

    pub async fn run(self, run: RunFlag, name: &str, jobs_dir: &Path) {
        // Check if there are no remnant child process after previous run.
        // If so, just kill it.
        let (cmd, args) = utils::bv_shell(&self.sh_body);
        utils::kill_all_processes(
            cmd,
            args.iter()
                .map(|item| item.as_str())
                .collect::<Vec<_>>()
                .as_slice(),
            self.shutdown_timeout,
            self.shutdown_signal,
        );
        <Self as JobRunner>::run(self, run, name, jobs_dir).await;
    }
}

#[async_trait]
impl<T: AsyncTimer + Send> JobRunnerImpl for RunShJob<T> {
    /// Run and restart job child process until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(self, mut run: RunFlag, name: &str) -> Result<(), JobStatus> {
        let (cmd_name, args) = utils::bv_shell(&self.sh_body);
        let mut cmd = Command::new(cmd_name);
        cmd.args(args.clone())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let args = args.iter().map(|item| item.as_str()).collect::<Vec<_>>();
        let mut backoff = JobBackoff::new(name, self.timer, run.clone(), &self.restart_policy);
        while run.load() {
            backoff.start();
            match cmd.spawn() {
                Ok(mut child) => {
                    info!("Spawned job '{name}'");
                    self.log_buffer
                        .attach(name, child.stdout.take(), child.stderr.take());
                    if let Some(exit_status) = run.select(child.wait()).await {
                        let message = format!("Job '{name}' finished with {exit_status:?}");
                        backoff
                            .stopped(exit_status.ok().and_then(|exit| exit.code()), message)
                            .await?;
                    } else {
                        info!("Job runner requested to stop, killing job '{name}'");
                        utils::kill_all_processes(
                            cmd_name,
                            args.as_slice(),
                            self.shutdown_timeout,
                            self.shutdown_signal,
                        );
                    }
                }
                Err(err) => {
                    backoff
                        .stopped(None, format!("Failed to spawn job '{name}': {err:#}"))
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
    use crate::jobs;
    use crate::jobs::{CONFIG_SUBDIR, STATUS_SUBDIR};
    use assert_fs::TempDir;
    use babel_api::engine::RestartConfig;
    use bv_utils::timer::MockAsyncTimer;
    use std::fs;
    use std::{io::Write, os::unix::fs::OpenOptionsExt};

    #[tokio::test]
    async fn test_run_with_logs() -> Result<()> {
        let job_name = "job_name".to_string();
        let tmp_root = TempDir::new()?.to_path_buf();
        let jobs_dir = tmp_root.join("jobs");
        fs::create_dir_all(jobs_dir.join(CONFIG_SUBDIR))?;
        fs::create_dir_all(jobs_dir.join(STATUS_SUBDIR))?;
        let test_run = RunFlag::default();
        let log_buffer = LogBuffer::new(16);
        let mut log_rx = log_buffer.subscribe();
        let cmd_path = tmp_root.join("test_cmd");
        {
            let mut cmd_file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o770)
                .open(&cmd_path)?;
            writeln!(cmd_file, "#!/bin/sh")?;
            writeln!(cmd_file, "echo 'cmd log'")?;
        }

        let mut timer_mock = MockAsyncTimer::new();
        let now = std::time::Instant::now();
        timer_mock.expect_now().returning(move || now);
        timer_mock.expect_sleep().returning(|_| ());
        RunShJob::new(
            timer_mock,
            cmd_path.to_string_lossy().to_string(),
            RestartPolicy::Always(RestartConfig {
                backoff_timeout_ms: 1000,
                backoff_base_ms: 100,
                max_retries: Some(3),
            }),
            Duration::from_secs(3),
            PosixSignal::SIGTERM,
            log_buffer,
        )?
        .run(test_run, &job_name, &jobs_dir)
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
