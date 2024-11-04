use crate::{download_job, upload_job};
use babel_api::engine::{Chunk, JobConfig, JobProgress, JobStatus, JobType, PROTOCOL_DATA_PATH};
use bv_utils::run_flag::RunFlag;
use chrono::{DateTime, Local};
use eyre::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::fs::File;
use std::io::{BufRead, Write};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use sysinfo::Pid;
use tokio::sync::Mutex;
use tracing::info;

lazy_static::lazy_static! {
    pub static ref JOBS_DIR: &'static Path = Path::new("/var/lib/babel/jobs");
    pub static ref ARCHIVE_JOBS_META_DIR: &'static Path = Path::new("/blockjoy/.babel_jobs");
}

pub const CONFIG_FILENAME: &str = "config.json";
pub const STATUS_FILENAME: &str = "status.json";
pub const PROGRESS_FILENAME: &str = "progress.json";
pub const LOGS_FILENAME: &str = "logs";
pub const LOG_EXPIRE_DAYS: i64 = 1;
pub const MAX_JOB_LOGS: usize = 1024;
pub const MAX_LOG_ENTRY_LEN: usize = 1024;

pub type JobsRegistry<C> = Arc<Mutex<JobsContext<C>>>;

pub struct JobsContext<C> {
    pub jobs: HashMap<String, Job>,
    pub jobs_dir: PathBuf,
    pub connector: C,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobState {
    Active(Pid),
    Inactive(JobStatus),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub job_dir: PathBuf,
    pub config: JobConfig,
    pub state: JobState,
    pub logs: Vec<(DateTime<Local>, String)>,
    pub restart_stamps: HashSet<DateTime<Local>>,
}

impl Job {
    pub fn new(job_dir: PathBuf, config: JobConfig, state: JobState) -> Self {
        Self {
            job_dir,
            config,
            state,
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

    pub fn save_config(&self) -> Result<()> {
        save_config(&self.config, &self.job_dir)
    }

    pub fn clear_status(&self) -> Result<()> {
        let path = self.job_dir.join(STATUS_FILENAME);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn save_status(&self) -> Result<()> {
        match &self.state {
            JobState::Active(_) => Ok(()),
            JobState::Inactive(status) => match status {
                JobStatus::Pending { .. } | JobStatus::Running => self.clear_status(),
                JobStatus::Finished { .. } | JobStatus::Stopped => {
                    save_status(status, &self.job_dir)
                }
            },
        }
    }

    pub fn load_status(&self) -> Result<JobStatus> {
        load_status(&self.job_dir)
    }

    pub fn load_progress(&self) -> Option<JobProgress> {
        load_job_data(&self.job_dir.join(PROGRESS_FILENAME)).ok()
    }

    pub fn cleanup(&self) -> Result<()> {
        match &self.config.job_type {
            JobType::Download { .. } => {
                download_job::cleanup_job(&ARCHIVE_JOBS_META_DIR, &PROTOCOL_DATA_PATH)?
            }
            JobType::Upload { .. } => upload_job::cleanup_job(&ARCHIVE_JOBS_META_DIR)?,
            _ => {}
        }
        Ok(())
    }
}

pub fn load_job_data<T: DeserializeOwned>(file_path: &Path) -> Result<T> {
    Ok(fs::read_to_string(file_path).and_then(|json| Ok(serde_json::from_str(&json)?))?)
}

pub fn save_job_data<T: Serialize>(file_path: &Path, data: &T) -> eyre::Result<()> {
    Ok(fs::write(file_path, serde_json::to_string(data)?)?)
}

pub fn load_config(job_dir: &Path) -> Result<JobConfig> {
    let path = job_dir.join(CONFIG_FILENAME);
    info!("Reading job config file: {}", path.display());
    fs::read_to_string(&path)
        .and_then(|s| serde_json::from_str::<JobConfig>(&s).map_err(Into::into))
        .with_context(|| format!("failed to read job config file `{}`", path.display()))
}

pub fn save_config(config: &JobConfig, job_dir: &Path) -> Result<()> {
    let path = job_dir.join(CONFIG_FILENAME);
    info!("Writing job config: {}", path.display());
    let config = serde_json::to_string(config)?;
    fs::write(&path, config)?;
    Ok(())
}

pub fn load_status(job_dir: &Path) -> Result<JobStatus> {
    let path = job_dir.join(STATUS_FILENAME);
    info!("Reading job status file: {}", path.display());
    fs::read_to_string(&path)
        .and_then(|s| serde_json::from_str::<JobStatus>(&s).map_err(Into::into))
        .with_context(|| format!("Failed to read job status file `{}`", path.display()))
}

pub fn save_status(status: &JobStatus, job_dir: &Path) -> Result<()> {
    let path = job_dir.join(STATUS_FILENAME);
    info!("Writing job status: {}", path.display());
    let status = serde_json::to_string(status)?;
    fs::write(&path, &status).with_context(|| {
        format!(
            "failed to save job status {status:?} to: {}",
            path.display(),
        )
    })
}

pub fn load_chunks(path: &Path) -> Result<Vec<Chunk>> {
    let mut chunks = vec![];
    if path.exists() {
        let file = File::open(path)?;
        for line in std::io::BufReader::new(file).lines() {
            chunks.push(serde_json::from_str::<Chunk>(&line?)?);
        }
    }
    Ok(chunks)
}

pub fn save_chunk(path: &Path, chunk: &Chunk) -> Result<()> {
    let mut file = fs::File::options().append(true).create(true).open(path)?;
    let mut chunk_serialized = serde_json::to_string(chunk)?;
    chunk_serialized.push('\n');
    file.write_all(chunk_serialized.as_bytes())
        .with_context(|| format!("{}:{chunk:?}", path.display()))?;
    Ok(())
}

pub struct RunnersState {
    pub result: Result<()>,
    pub run: RunFlag,
}

impl RunnersState {
    pub fn handle_error(&mut self, err: eyre::Report) {
        if self.result.is_ok() {
            self.result = Err(err);
            self.run.stop();
        }
    }
}
