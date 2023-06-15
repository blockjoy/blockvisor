/// This module implements job runner for fetching blockchain data. It downloads blockchain data
/// according to to given manifest and destination dir. In case of recoverable errors download
/// is retried according to given `RestartPolicy`, with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset if download continue without errors for at least `backoff_timeout_ms`.
use crate::job_runner::JobRunnerImpl;
use async_trait::async_trait;
use babel_api::engine::{BlockchainDataManifest, JobStatus, RestartPolicy};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{bail, Result};
use std::path::PathBuf;

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
impl<T: AsyncTimer + Send> JobRunnerImpl for FetchBlockchain<T> {
    /// Run and restart job child process until `backoff.stopped` return `JobStatus` or job runner
    /// is stopped explicitly.  
    async fn try_run_job(&mut self, _run: RunFlag, _name: &str) -> Result<(), JobStatus> {
        Ok(())
    }
}
