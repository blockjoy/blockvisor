/// This module implements job runner for fetching blockchain data. It downloads blockchain data
/// according to to given manifest and destination dir. In case of recoverable errors download
/// is retried according to given `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if download continue without errors for at least `backoff_timeout_ms`.
use crate::{job_runner::JobRunnerImpl, jobs, jobs::STATUS_SUBDIR, utils};
use async_trait::async_trait;
use babel_api::engine::{BlockchainDataManifest, JobStatus, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{bail, Result};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::{debug, error, info, warn};

pub struct FetchBlockchain<T> {
    manifest: BlockchainDataManifest,
    destination: PathBuf,
    restart_policy: RestartPolicy,
    timer: T,
}
impl<T: AsyncTimer> FetchBlockchain<T> {
    pub fn new(
        timer: T,
        manifest: BlockchainDataManifest,
        destination: PathBuf,
        restart_policy: RestartPolicy,
    ) -> Result<Self> {
        if let RestartPolicy::Always(_) = &restart_policy {
            bail!("'RestartPolicy::Always' is not allowed for 'FetchBlockchain' job")
        }
        Ok(Self {
            manifest,
            destination,
            restart_policy,
            timer,
        })
    }
}

#[async_trait]
impl<T: Send> JobRunnerImpl for FetchBlockchain<T> {
    /// Run and restart job child process until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(&mut self, mut run: RunFlag, name: &str) -> Result<(), JobStatus> {
        // let (cmd, args) = utils::bv_shell(&self.sh_body);
        // let mut cmd = Command::new(cmd);
        // cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        // let mut backoff = JobBackoff::new(&self.timer, run.clone(), &self.restart_policy);
        // while run.load() {
        //     backoff.start();
        //     match cmd.spawn() {
        //         Ok(mut child) => {
        //             info!("Spawned job '{job_name}'");
        //             self.log_buffer
        //                 .attach(job_name, child.stdout.take(), child.stderr.take());
        //             tokio::select!(
        //                 exit_status = child.wait() => {
        //                     let message = format!("Job '{job_name}' finished with {exit_status:?}");
        //                     backoff
        //                         .stopped(exit_status.ok().and_then(|exit| exit.code()), message)
        //                         .await?;
        //                 },
        //                 _ = run.wait() => {
        //                     info!("Job runner requested to stop, killing job '{job_name}'");
        //                     let _ = child.kill().await;
        //                 },
        //             );
        //         }
        //         Err(err) => {
        //             backoff
        //                 .stopped(None, format!("Failed to spawn job '{job_name}': {err}"))
        //                 .await?;
        //         }
        //     }
        // }
        Ok(())
    }
}
//
// #[cfg(test)]
// mod tests {
//     use super::*;
//     use crate::jobs::CONFIG_SUBDIR;
//     use assert_fs::TempDir;
//     use bv_utils::timer::MockAsyncTimer;
//     use std::fs;
//     use std::{io::Write, os::unix::fs::OpenOptionsExt};
//
//     #[tokio::test]
//     async fn test_stopped_restart_never() -> Result<()> {
//         let test_run = RunFlag::default();
//         let timer_mock = MockAsyncTimer::new();
//         let mut backoff = JobBackoff::new(&timer_mock, test_run, &RestartPolicy::Never);
//         backoff.start(); // should do nothing
//         assert_eq!(
//             JobStatus::Finished {
//                 exit_code: None,
//                 message: "test message".to_string()
//             },
//             backoff
//                 .stopped(None, "test message".to_owned())
//                 .await
//                 .unwrap_err()
//         );
//         assert_eq!(
//             JobStatus::Finished {
//                 exit_code: Some(0),
//                 message: "".to_string()
//             },
//             backoff
//                 .stopped(Some(0), "test message".to_owned())
//                 .await
//                 .unwrap_err()
//         );
//         Ok(())
//     }
//
//     #[tokio::test]
//     async fn test_stopped_restart_always() -> Result<()> {
//         let test_run = RunFlag::default();
//         let mut timer_mock = MockAsyncTimer::new();
//         let now = std::time::Instant::now();
//         timer_mock.expect_now().returning(move || now);
//         timer_mock.expect_sleep().returning(|_| ());
//
//         let mut backoff = JobBackoff::new(
//             &timer_mock,
//             test_run,
//             &RestartPolicy::Always(RestartConfig {
//                 backoff_timeout_ms: 1000,
//                 backoff_base_ms: 100,
//                 max_retries: Some(1),
//             }),
//         );
//         backoff.start();
//         backoff
//             .stopped(Some(0), "test message".to_owned())
//             .await
//             .unwrap();
//         assert_eq!(
//             JobStatus::Finished {
//                 exit_code: Some(1),
//                 message: "test message".to_string()
//             },
//             backoff
//                 .stopped(Some(1), "test message".to_owned())
//                 .await
//                 .unwrap_err()
//         );
//         Ok(())
//     }
//
//     #[tokio::test]
//     async fn test_stopped_restart_on_failure() -> Result<()> {
//         let test_run = RunFlag::default();
//         let mut timer_mock = MockAsyncTimer::new();
//         let now = std::time::Instant::now();
//         timer_mock.expect_now().returning(move || now);
//         timer_mock.expect_sleep().returning(|_| ());
//
//         let mut backoff = JobBackoff::new(
//             &timer_mock,
//             test_run,
//             &RestartPolicy::OnFailure(RestartConfig {
//                 backoff_timeout_ms: 1000,
//                 backoff_base_ms: 100,
//                 max_retries: Some(1),
//             }),
//         );
//         backoff.start();
//         backoff
//             .stopped(Some(1), "test message".to_owned())
//             .await
//             .unwrap();
//         assert_eq!(
//             JobStatus::Finished {
//                 exit_code: Some(1),
//                 message: "test message".to_string()
//             },
//             backoff
//                 .stopped(Some(1), "test message".to_owned())
//                 .await
//                 .unwrap_err()
//         );
//         assert_eq!(
//             JobStatus::Finished {
//                 exit_code: Some(0),
//                 message: "".to_string()
//             },
//             backoff
//                 .stopped(Some(0), "test message".to_owned())
//                 .await
//                 .unwrap_err()
//         );
//         Ok(())
//     }
//
//     #[tokio::test]
//     async fn test_run_with_logs() -> Result<()> {
//         let job_name = "job_name".to_string();
//         let tmp_root = TempDir::new()?.to_path_buf();
//         let jobs_dir = tmp_root.join("jobs");
//         fs::create_dir_all(&jobs_dir.join(CONFIG_SUBDIR))?;
//         fs::create_dir_all(&jobs_dir.join(STATUS_SUBDIR))?;
//         let test_run = RunFlag::default();
//         let log_buffer = LogBuffer::new(16);
//         let mut log_rx = log_buffer.subscribe();
//         let cmd_path = tmp_root.join("test_cmd");
//         {
//             let mut cmd_file = fs::OpenOptions::new()
//                 .create(true)
//                 .write(true)
//                 .mode(0o770)
//                 .open(&cmd_path)?;
//             writeln!(cmd_file, "#!/bin/sh")?;
//             writeln!(cmd_file, "echo 'cmd log'")?;
//         }
//
//         let mut timer_mock = MockAsyncTimer::new();
//         let now = std::time::Instant::now();
//         timer_mock.expect_now().returning(move || now);
//         timer_mock.expect_sleep().returning(|_| ());
//         RunSh::new(
//             timer_mock,
//             cmd_path.to_string_lossy().to_string(),
//             RestartPolicy::Always(RestartConfig {
//                 backoff_timeout_ms: 1000,
//                 backoff_base_ms: 100,
//                 max_retries: Some(3),
//             }),
//             &jobs_dir,
//             job_name.clone(),
//             log_buffer,
//         )?
//         .run(test_run)
//         .await;
//
//         let status = jobs::load_status(&jobs::status_file_path(
//             &job_name,
//             &jobs_dir.join(STATUS_SUBDIR),
//         ))?;
//         assert_eq!(
//             status,
//             JobStatus::Finished {
//                 exit_code: Some(0),
//                 message: "Job 'job_name' finished with Ok(ExitStatus(unix_wait_status(0)))"
//                     .to_string()
//             }
//         );
//         assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // first start
//         assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // retry 1
//         assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // retry 2
//         assert!(log_rx.recv().await?.ends_with("|job_name|cmd log\n")); // retry 3
//         log_rx.try_recv().unwrap_err();
//         Ok(())
//     }
// }
