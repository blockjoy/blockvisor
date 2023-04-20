use babel_api::engine::{JobConfig, JobStatus};
use eyre::{Context, Result};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use sysinfo::Pid;
use tokio::sync::Mutex;
use tracing::info;

lazy_static::lazy_static! {
    pub static ref JOBS_DIR: &'static Path = Path::new("/var/lib/babel/jobs");
}

pub const CONFIG_SUBDIR: &str = "config";
pub const STATUS_SUBDIR: &str = "status";

pub type JobsRegistry = Arc<Mutex<Jobs>>;

pub type Jobs = (HashMap<String, Job>, JobsData);

#[derive(Clone, Debug, PartialEq)]
pub struct JobsData {
    jobs_config_dir: PathBuf,
    jobs_status_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobState {
    Active(Pid),
    Inactive(JobStatus),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub state: JobState,
    pub config: JobConfig,
}

pub fn load_config(path: &Path) -> Result<JobConfig> {
    info!("Reading job config file: {}", path.display());
    fs::read_to_string(path)
        .and_then(|s| serde_json::from_str::<JobConfig>(&s).map_err(Into::into))
        .with_context(|| format!("failed to read job config file `{}`", path.display()))
}

pub fn save_config(config: &JobConfig, name: &str, jobs_config_dir: &Path) -> Result<()> {
    let path = config_file_path(name, jobs_config_dir);
    info!("Writing job config: {}", path.display());
    let config = serde_json::to_string(config)?;
    fs::write(&path, config)?;
    Ok(())
}

pub fn config_file_path(name: &str, jobs_config_dir: &Path) -> PathBuf {
    let filename = format!("{}.cfg", name);
    jobs_config_dir.join(filename)
}

pub fn load_status(path: &Path) -> Result<JobStatus> {
    info!("Reading job status file: {}", path.display());
    fs::read_to_string(path)
        .and_then(|s| serde_json::from_str::<JobStatus>(&s).map_err(Into::into))
        .with_context(|| format!("Failed to read job status file `{}`", path.display()))
}

pub fn save_status(status: &JobStatus, name: &str, jobs_status_dir: &Path) -> Result<()> {
    let path = status_file_path(name, jobs_status_dir);
    info!("Writing job status: {}", path.display());
    let status = serde_json::to_string(status)?;
    fs::write(&path, &status)
        .with_context(|| format!("failed to save job '{}' status {:?}", name, status))
}

pub fn status_file_path(name: &str, jobs_status_dir: &Path) -> PathBuf {
    let filename = format!("{}.status", name);
    jobs_status_dir.join(filename)
}

impl JobsData {
    pub fn new(jobs_dir: &Path) -> Self {
        Self {
            jobs_config_dir: jobs_dir.join(CONFIG_SUBDIR),
            jobs_status_dir: jobs_dir.join(STATUS_SUBDIR),
        }
    }

    pub fn clear_status(&self, name: &str) {
        let _ = fs::remove_file(status_file_path(name, &self.jobs_status_dir));
    }

    pub fn save_config(&self, config: &JobConfig, name: &str) -> Result<()> {
        save_config(config, name, &self.jobs_config_dir)
    }

    pub fn save_status(&self, status: &JobStatus, name: &str) -> Result<()> {
        save_status(status, name, &self.jobs_status_dir)
    }

    pub fn load_status(&self, name: &str) -> Result<JobStatus> {
        load_status(&status_file_path(name, &self.jobs_status_dir))
    }
}
