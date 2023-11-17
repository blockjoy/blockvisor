use crate::download_job;
use babel_api::engine::{JobConfig, JobProgress, JobStatus, JobType};
use chrono::{DateTime, Local};
use eyre::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use sysinfo::Pid;
use tokio::sync::Mutex;
use tracing::{info, warn};

lazy_static::lazy_static! {
    pub static ref JOBS_DIR: &'static Path = Path::new("/var/lib/babel/jobs");
}

pub const CONFIG_SUBDIR: &str = "config";
pub const STATUS_SUBDIR: &str = "status";
pub const LOG_EXPIRE_DAYS: i64 = 1;
pub const MAX_JOB_LOGS: usize = 1024;
pub const MAX_LOG_ENTRY_LEN: usize = 1024;

pub type JobsRegistry = Arc<Mutex<Jobs>>;

pub type Jobs = (HashMap<String, Job>, JobsData);

#[derive(Clone, Debug, PartialEq)]
pub struct JobsData {
    pub jobs_config_dir: PathBuf,
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
    pub logs: Vec<(DateTime<Local>, String)>,
    pub restart_stamps: HashSet<DateTime<Local>>,
}

impl Job {
    pub fn new(state: JobState, config: JobConfig) -> Self {
        Self {
            state,
            config,
            logs: Default::default(),
            restart_stamps: Default::default(),
        }
    }

    /// Add restart timestamp to `Job` internal state.
    pub fn register_restart(&mut self) {
        let time = self.update();
        self.restart_stamps.insert(time);
    }

    /// Push log message with timestamp to `Job` internal state.
    pub fn push_log(&mut self, message: &str) {
        let time = self.update();
        if let Some((index, _)) = message.char_indices().nth(MAX_LOG_ENTRY_LEN) {
            message.to_string().truncate(index);
        }
        self.logs.push((time, format!("{time}| {message}")));
    }

    /// Update `Job` internal state, by removing outdated logs and restart stamps.
    /// Return `now()` time for convenience.
    pub fn update(&mut self) -> DateTime<Local> {
        let time = Local::now();
        let not_old = |timestamp: &DateTime<Local>| {
            time.signed_duration_since(*timestamp).num_days() < LOG_EXPIRE_DAYS
        };
        self.restart_stamps.retain(not_old);
        self.logs.retain(|(timestamp, _)| not_old(timestamp));
        if self.logs.len() >= MAX_JOB_LOGS {
            self.logs = self.logs.split_off(MAX_JOB_LOGS - 1);
        }
        time
    }
}

impl JobsData {
    pub fn new(jobs_dir: &Path) -> Self {
        Self {
            jobs_config_dir: jobs_dir.join(CONFIG_SUBDIR),
            jobs_status_dir: jobs_dir.join(STATUS_SUBDIR),
        }
    }

    pub fn save_config(&self, config: &JobConfig, name: &str) -> Result<()> {
        save_config(config, name, &self.jobs_config_dir)
    }

    pub fn clear_status(&self, name: &str) {
        let _ = fs::remove_file(status_file_path(name, &self.jobs_status_dir));
    }

    pub fn save_status(&self, status: &JobStatus, name: &str) -> Result<()> {
        save_status(status, name, &self.jobs_status_dir)
    }

    pub fn load_status(&self, name: &str) -> Result<JobStatus> {
        load_status(&status_file_path(name, &self.jobs_status_dir))
    }

    pub fn load_progress(&self, name: &str) -> Option<JobProgress> {
        load_job_data(&progress_file_path(name, &self.jobs_status_dir))
    }

    pub fn cleanup_job(&self, name: &str, config: &JobConfig) {
        match &config.job_type {
            JobType::Download {
                manifest,
                destination,
                ..
            } => download_job::cleanup_job(
                &parts_file_path(name, &self.jobs_status_dir),
                manifest
                    .as_ref()
                    .map(|manifest| (destination.as_path(), &manifest.chunks)),
            ),
            JobType::Upload { .. } => {
                cleanup_job_data(&parts_file_path(name, &self.jobs_status_dir))
            }
            _ => {}
        }
    }
}

pub fn load_job_data<T: DeserializeOwned + Default>(file_path: &Path) -> T {
    if file_path.exists() {
        fs::read_to_string(file_path)
            .and_then(|json| Ok(serde_json::from_str(&json)?))
            .unwrap_or_default()
    } else {
        Default::default()
    }
}

pub fn save_job_data<T: Serialize>(file_path: &Path, data: &T) -> eyre::Result<()> {
    Ok(fs::write(file_path, serde_json::to_string(data)?)?)
}

pub fn cleanup_job_data(file_path: &Path) {
    if let Err(err) = fs::remove_file(file_path) {
        warn!(
            "failed to cleanup job data file `{}`: {err:#}",
            file_path.display()
        );
    }
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

pub fn config_file_path(name: &str, jobs_config_dir: &Path) -> PathBuf {
    let filename = format!("{}.cfg", name);
    jobs_config_dir.join(filename)
}

pub fn status_file_path(name: &str, jobs_status_dir: &Path) -> PathBuf {
    let filename = format!("{}.status", name);
    jobs_status_dir.join(filename)
}

pub fn progress_file_path(name: &str, jobs_status_dir: &Path) -> PathBuf {
    let filename = format!("{}.progress", name);
    jobs_status_dir.join(filename)
}

pub fn parts_file_path(name: &str, jobs_status_dir: &Path) -> PathBuf {
    let filename = format!("{}.parts", name);
    jobs_status_dir.join(filename)
}
