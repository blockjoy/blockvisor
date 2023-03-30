use babel_api::config::{JobConfig, JobStatus};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

lazy_static::lazy_static! {
    pub static ref JOBS_DIR: &'static Path = Path::new("/var/lib/babel/jobs");
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobData {
    pub config: JobConfig,
    pub status: JobStatus,
}

impl JobData {
    pub fn load(path: &Path) -> Result<Self> {
        info!("Reading job data file: {}", path.display());
        fs::read_to_string(path)
            .and_then(|s| serde_json::from_str::<Self>(&s).map_err(Into::into))
            .with_context(|| format!("Failed to read job data file `{}`", path.display()))
    }

    pub fn set_status(&mut self, value: JobStatus, jobs_dir: &Path, name: &str) -> Result<()> {
        self.status = value;
        self.save(jobs_dir, name).with_context(|| {
            format!(
                "failed to set job '{}' status to {:?}, can't save job data to file",
                name, self.status
            )
        })
    }

    pub fn save(&self, jobs_dir: &Path, name: &str) -> Result<()> {
        let path = Self::file_path(name, jobs_dir);
        info!("Writing job data: {}", path.display());
        let config = serde_json::to_string(self)?;
        fs::write(&path, &*config)?;

        Ok(())
    }

    pub fn file_path(name: &str, jobs_dir: &Path) -> PathBuf {
        let filename = format!("{}.cfg", name);
        jobs_dir.join(filename)
    }
}
