use crate::{
    async_pid_watch::AsyncPidWatch,
    babel_service::JobRunnerLock,
    job_data::JobData,
    utils::{find_process, kill_all},
};
use async_trait::async_trait;
use babel_api::config::{JobConfig, JobStatus};
use bv_utils::run_flag::RunFlag;
use eyre::{bail, Context, ContextCompat, Report, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{
    collections::HashMap,
    fs::read_dir,
    path::{Path, PathBuf},
    sync::Arc,
};
use sysinfo::{Pid, PidExt, Process, System, SystemExt};
use tokio::{
    select,
    sync::{watch, Mutex, MutexGuard},
};

pub fn create(
    jobs_dir: &Path,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: &Path,
) -> Result<(Client, Monitor)> {
    if !jobs_dir.exists() {
        std::fs::create_dir_all(jobs_dir)?;
    }
    let jobs_registry = Arc::new(Mutex::new(load_jobs(jobs_dir, job_runner_bin_path)?));
    let (change_tx, change_rx) = watch::channel(());
    Ok((
        Client {
            jobs_dir: jobs_dir.to_path_buf(),
            jobs_registry: jobs_registry.clone(),
            job_runner_bin_path: job_runner_bin_path.to_string_lossy().to_string(),
            change_tx,
        },
        Monitor {
            jobs_dir: jobs_dir.to_path_buf(),
            jobs_registry,
            job_runner_lock,
            job_runner_bin_path: job_runner_bin_path.to_string_lossy().to_string(),
            change_rx,
        },
    ))
}

fn load_jobs(jobs_dir: &Path, job_runner_bin_path: &Path) -> Result<HashMap<String, Job>> {
    let mut jobs = HashMap::new();
    let dir = read_dir(jobs_dir).with_context(|| "failed to read jobs from dir {jobs_dir}")?;
    let mut sys = System::new();
    sys.refresh_processes();
    let ps = sys.processes();
    for entry in dir {
        let path = entry
            .with_context(|| "failed to read jobs registry entry")?
            .path();
        if let Some(name) = path.file_stem() {
            let name = name.to_string_lossy().to_string();
            if let Some((pid, _)) =
                find_process(&job_runner_bin_path.to_string_lossy(), &[&name], ps).first()
            {
                jobs.insert(name.clone(), Job::Active(**pid));
            } else {
                jobs.insert(name, Job::Inactive(JobData::load(&path)?));
            }
        }
    }
    Ok(jobs)
}

#[async_trait]
pub trait JobsManagerClient {
    async fn start(&self, name: &str, config: JobConfig) -> Result<()>;
    async fn stop(&self, name: &str) -> Result<()>;
    async fn status(&self, name: &str) -> Result<JobStatus>;
}

#[derive(Clone)]
enum Job {
    Active(Pid),
    Inactive(JobData),
}
type JobsRegistry = Arc<Mutex<HashMap<String, Job>>>;

pub struct Client {
    jobs_dir: PathBuf,
    jobs_registry: JobsRegistry,
    job_runner_bin_path: String,
    change_tx: watch::Sender<()>,
}

#[async_trait]
impl JobsManagerClient for Client {
    async fn start(&self, name: &str, config: JobConfig) -> Result<()> {
        let mut jobs = self.jobs_registry.lock().await;
        if let Some(Job::Active(_)) = jobs.get(name) {
            bail!("can't start, job '{name}' is already running")
        }
        let data = JobData {
            config,
            status: JobStatus::Pending,
        };
        data.save(&self.jobs_dir, name)
            .with_context(|| format!("failed to start job {name}, can't save job data to file"))?;
        jobs.insert(name.to_string(), Job::Inactive(data));
        let _ = self.change_tx.send(());
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        if let Some(job) = self.jobs_registry.lock().await.get_mut(name) {
            match job {
                Job::Active(_) => {
                    let mut sys = System::new();
                    sys.refresh_processes();
                    let ps = sys.processes();
                    kill_all(&self.job_runner_bin_path, &[name], ps);
                    let mut data = JobData::load(&JobData::file_path(name, &self.jobs_dir))
                        .with_context(|| {
                            format!("failed to stop job {name}, can't load job data from file")
                        })?;
                    data.set_status(JobStatus::Stopped, &self.jobs_dir, name)?;
                    *job = Job::Inactive(data);
                }
                Job::Inactive(data) => {
                    data.set_status(JobStatus::Stopped, &self.jobs_dir, name)?;
                }
            }
        } else {
            bail!("can't stop, job '{name}' not found")
        }
        Ok(())
    }

    async fn status(&self, name: &str) -> Result<JobStatus> {
        Ok(
            if let Job::Inactive(data) = self
                .jobs_registry
                .lock()
                .await
                .get(name)
                .with_context(|| format!("unknown status, job '{name}' not found"))?
            {
                data.status.clone()
            } else {
                JobStatus::Running
            },
        )
    }
}
pub struct Monitor {
    jobs_dir: PathBuf,
    jobs_registry: JobsRegistry,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: String,
    change_rx: watch::Receiver<()>,
}

impl Monitor {
    pub async fn run(mut self, mut run: RunFlag) {
        let mut sys = System::new();
        while run.load() {
            sys.refresh_processes();
            if let Ok(async_pids) = self.update_jobs(sys.processes()).await {
                if async_pids.is_empty() {
                    // no jobs :( - just wait for change
                    select!(
                        _ = self.change_rx.changed() => {}
                        _ = run.wait() => {}
                    );
                } else {
                    let mut futures = FuturesUnordered::new();
                    for apid in &async_pids {
                        futures.push(apid.watch());
                    }
                    select!(
                        _ = futures.next() => {}
                        _ = self.change_rx.changed() => {}
                        _ = run.wait() => {}
                    );
                }
            } // refresh process and update_jobs again in case of error
        }
    }

    async fn update_jobs(&mut self, ps: &HashMap<Pid, Process>) -> Result<Vec<AsyncPidWatch>> {
        let mut jobs = self.jobs_registry.lock().await;
        self.update_active_jobs(ps, &mut jobs).await;
        self.check_inactive_jobs(&mut jobs).await;

        let mut async_pids = vec![];
        for (_, job) in jobs.iter() {
            if let Job::Active(pid) = job {
                async_pids.push(AsyncPidWatch::new(*pid)?);
            }
        }
        Ok(async_pids)
    }

    async fn update_active_jobs(
        &self,
        ps: &HashMap<Pid, Process>,
        jobs: &mut MutexGuard<'_, HashMap<String, Job>>,
    ) {
        for (name, job) in jobs.iter_mut() {
            if let Job::Active(job_pid) = job {
                if !ps.iter().any(|(pid, _)| pid == job_pid) {
                    *job = self.handle_stopped_job(name).await;
                }
            }
        }
    }

    async fn check_inactive_jobs(&self, jobs: &mut MutexGuard<'_, HashMap<String, Job>>) {
        let deps = jobs.clone();
        for (name, job) in jobs.iter_mut() {
            if let Job::Inactive(JobData {
                config: JobConfig { needs, .. },
                status,
            }) = job
            {
                if *status == JobStatus::Pending {
                    let deps_finished = deps_finished(name, &deps, needs);
                    match deps_finished {
                        Ok(true) => {
                            match self.start_job(name).await {
                                Ok(pid) => *job = Job::Active(pid),
                                Err(err) => {
                                    *status = JobStatus::Finished {
                                        exit_code: None,
                                        message: format!("failed to start job {name}: {err}"),
                                    }
                                }
                            };
                        }
                        Err(err) => {
                            *status = JobStatus::Finished {
                                exit_code: None,
                                message: err.to_string(),
                            };
                        }
                        Ok(false) => {}
                    }
                }
            }
        }
    }

    async fn handle_stopped_job(&self, name: &str) -> Job {
        let mut data = JobData::load(&JobData::file_path(name, &self.jobs_dir))
            .unwrap_or_else(|err|panic!("can't load job '{name}' data from file after it stopped - this should never happen - panic! Cause: {err}"));
        if let JobStatus::Finished { .. } = data.status {
            Job::Inactive(data)
        } else {
            // job process ended, but job was not finished - try restart
            match self.start_job(name).await {
                Ok(pid) => Job::Active(pid),
                Err(err) => {
                    data.status = JobStatus::Finished {
                        exit_code: None,
                        message: format!("failed to restart job {name}: {err}"),
                    };
                    Job::Inactive(data)
                }
            }
        }
    }

    async fn start_job(&self, name: &str) -> Result<Pid> {
        // make sure job runner binary is not currently updated
        let lock = self.job_runner_lock.read().await;
        if lock.is_some() {
            let child = tokio::process::Command::new(&self.job_runner_bin_path)
                .args([&name])
                .spawn()?;
            Ok(Pid::from_u32(child.id().with_context(|| {
                format!("can't get PID of started job '{name}'")
            })?))
        } else {
            bail!("missing job runner binary");
        }
    }
}

fn deps_finished(
    name: &str,
    deps: &HashMap<String, Job>,
    needs: &mut [String],
) -> Result<bool, Report> {
    for needed_name in needs {
        match deps
            .get(needed_name)
            .with_context(|| format!("job '{name}' needs '{needed_name}', but it is not defined"))?
        {
            Job::Active(_) => return Ok(false),
            Job::Inactive(JobData {
                status: JobStatus::Stopped,
                ..
            }) => bail!("job '{name}' needs '{needed_name}', but it was stopped"),
            Job::Inactive(JobData {
                status:
                    JobStatus::Finished {
                        exit_code: Some(0), ..
                    },
                ..
            }) => {}
            Job::Inactive(JobData {
                status: JobStatus::Finished { exit_code, message },
                ..
            }) => bail!(
                "job '{name}' needs '{needed_name}', but it failed with {exit_code:?} - {message}"
            ),
            Job::Inactive(JobData { .. }) => return Ok(false),
        }
    }
    Ok(true)
}
