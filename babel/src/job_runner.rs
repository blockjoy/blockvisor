use crate::job_data::JobData;
/// This module implements job runner for for long running jobs. This includes long initialization tasks
/// and blockchain entrypoints as well, dependent on restart policy. It spawn child process as defined in
/// given config and watch it. Stopped child (whatever reason) is respawned according to given config,
/// with exponential backoff timeout and max retries (if configured).
/// Backoff timeout and retry count are reset after child stays alive for at least `backoff_timeout_ms`.
use bv_utils::run_flag::RunFlag;
use bv_utils::timer::Timer;
use eyre::Context;
use eyre::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct JobRunner<T> {
    jobs_dir: PathBuf,
    job_data: JobData,
    timer: T,
}

impl<T: Timer> JobRunner<T> {
    pub fn new(timer: T, jobs_dir: &Path, name: String) -> Result<Self> {
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
        })
    }

    pub async fn run(&self, mut run: RunFlag) {
        // TODO implement
        let _ = &self.jobs_dir;
        let _ = &self.job_data;
        self.timer.sleep(Duration::from_secs(1));
        run.wait().await
    }
}

#[cfg(test)]
mod tests {}
