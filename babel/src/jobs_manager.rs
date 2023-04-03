/// Jobs Manager consists of two parts:
/// .1 Client - allow asynchronous operations on jobs: start, stop and get status
/// .2 Monitor - background worker that monitor job runners and take proper actions when some job runner ends
/// It also start/stop requested jobs.
use crate::{
    async_pid_watch::AsyncPidWatch,
    babel_service::JobRunnerLock,
    jobs,
    jobs::{Job, JobState, Jobs, JobsData, JobsRegistry, CONFIG_SUBDIR, STATUS_SUBDIR},
    utils::{find_processes, kill_all_processes},
};
use async_trait::async_trait;
use babel_api::config::{JobConfig, JobStatus, RestartPolicy};
use bv_utils::run_flag::RunFlag;
use eyre::{bail, Context, ContextCompat, Report, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashMap, fs, fs::read_dir, path::Path, sync::Arc};
use sysinfo::{Pid, PidExt, Process, System, SystemExt};
use tokio::{
    select,
    sync::{watch, Mutex},
};
use tracing::error;

pub fn create(
    jobs_dir: &Path,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: &Path,
) -> Result<(Client, Monitor)> {
    let jobs_config_dir = jobs_dir.join(CONFIG_SUBDIR);
    if !jobs_config_dir.exists() {
        fs::create_dir_all(jobs_config_dir)?;
    }
    let jobs_status_dir = jobs_dir.join(STATUS_SUBDIR);
    if !jobs_status_dir.exists() {
        fs::create_dir_all(jobs_status_dir)?;
    }
    let jobs_registry = Arc::new(Mutex::new(load_jobs(jobs_dir, job_runner_bin_path)?));
    let (job_added_tx, job_added_rx) = watch::channel(());
    Ok((
        Client {
            jobs_registry: jobs_registry.clone(),
            job_runner_bin_path: job_runner_bin_path.to_string_lossy().to_string(),
            job_added_tx,
        },
        Monitor {
            jobs_registry,
            job_runner_lock,
            job_runner_bin_path: job_runner_bin_path.to_string_lossy().to_string(),
            job_added_rx,
        },
    ))
}

fn load_jobs(jobs_dir: &Path, job_runner_bin_path: &Path) -> Result<Jobs> {
    let mut jobs = HashMap::new();
    let jobs_data = JobsData::new(jobs_dir);
    let jobs_config_dir = jobs_dir.join(CONFIG_SUBDIR);
    let dir = read_dir(&jobs_config_dir)
        .with_context(|| format!("failed to read jobs from dir {}", jobs_config_dir.display()))?;
    let mut sys = System::new();
    sys.refresh_processes();
    let ps = sys.processes();
    for entry in dir {
        let path = entry
            .with_context(|| "failed to read jobs registry entry")?
            .path();
        if let Some(name) = path.file_stem() {
            let name = name.to_string_lossy().to_string();
            match jobs::load_config(&path) {
                Ok(config) => {
                    let state = if let Some((pid, _)) =
                        find_processes(&job_runner_bin_path.to_string_lossy(), &[&name], ps).next()
                    {
                        JobState::Active(*pid)
                    } else {
                        JobState::Inactive(
                            jobs_data.load_status(&name).unwrap_or(JobStatus::Pending),
                        )
                    };
                    jobs.insert(name, Job { state, config });
                }
                Err(err) => {
                    // invalid job config file log error, remove invalid file and go to next one
                    error!(
                        "invalid job '{}' config file {}, load failed with: {}",
                        name,
                        path.display(),
                        err
                    );
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
    Ok((jobs, jobs_data))
}

#[async_trait]
pub trait JobsManagerClient {
    async fn start(&self, name: &str, config: JobConfig) -> Result<()>;
    async fn stop(&self, name: &str) -> Result<()>;
    async fn status(&self, name: &str) -> Result<JobStatus>;
}

pub struct Client {
    jobs_registry: JobsRegistry,
    job_runner_bin_path: String,
    job_added_tx: watch::Sender<()>,
}

#[async_trait]
impl JobsManagerClient for Client {
    async fn start(&self, name: &str, config: JobConfig) -> Result<()> {
        let mut lock = self.jobs_registry.lock().await;
        let (jobs, jobs_data) = &mut *lock;
        if let Some(Job {
            state: JobState::Active(_),
            config: old_config,
        }) = jobs.get(name)
        {
            if config != *old_config {
                bail!("can't start, job '{name}' is already running with different config")
            }
        }

        jobs_data.clear_status(name);
        jobs_data.save_config(&config, name).with_context(|| {
            format!("failed to start job {name}, can't save job config to file")
        })?;
        jobs.insert(
            name.to_string(),
            Job {
                state: JobState::Inactive(JobStatus::Pending),
                config,
            },
        );
        let _ = self.job_added_tx.send(());
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let mut lock = self.jobs_registry.lock().await;
        let (jobs, jobs_data) = &mut *lock;
        if let Some(job) = jobs.get_mut(name) {
            match &mut job.state {
                JobState::Active(_) => {
                    let mut sys = System::new();
                    sys.refresh_processes();
                    let ps = sys.processes();
                    kill_all_processes(&self.job_runner_bin_path, &[name], ps);
                    job.state = JobState::Inactive(JobStatus::Stopped);
                }
                JobState::Inactive(status) => {
                    *status = JobStatus::Stopped;
                }
            }
            jobs_data.save_status(&JobStatus::Stopped, name)?;
        } else {
            bail!("can't stop, job '{name}' not found")
        }
        Ok(())
    }

    async fn status(&self, name: &str) -> Result<JobStatus> {
        let lock = self.jobs_registry.lock().await;
        let (jobs, _) = &*lock;
        Ok(
            if let Job {
                state: JobState::Inactive(status),
                ..
            } = jobs
                .get(name)
                .with_context(|| format!("unknown status, job '{name}' not found"))?
            {
                status.clone()
            } else {
                JobStatus::Running
            },
        )
    }
}

pub struct Monitor {
    jobs_registry: JobsRegistry,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: String,
    job_added_rx: watch::Receiver<()>,
}

impl Monitor {
    pub async fn run(mut self, mut run: RunFlag) {
        let mut sys = System::new();
        while run.load() {
            sys.refresh_processes();
            if let Ok(async_pids) = self.update_jobs(sys.processes()).await {
                if async_pids.is_empty() {
                    // no jobs :( - just wait for job to be added
                    select!(
                        _ = self.job_added_rx.changed() => {}
                        _ = run.wait() => {}
                    );
                } else {
                    let mut futures: FuturesUnordered<_> =
                        async_pids.iter().map(|a| a.watch()).collect();
                    select!(
                        _ = futures.next() => {}
                        _ = self.job_added_rx.changed() => {}
                        _ = run.wait() => {}
                    );
                }
            } // refresh process and update_jobs again in case of error
        }
    }

    async fn update_jobs(&mut self, ps: &HashMap<Pid, Process>) -> Result<Vec<AsyncPidWatch>> {
        let mut lock = self.jobs_registry.lock().await;
        let (jobs, jobs_data) = &mut *lock;
        self.update_active_jobs(ps, jobs, jobs_data).await;
        self.check_inactive_jobs(jobs, jobs_data).await;

        let mut async_pids = vec![];
        for job in jobs.values() {
            if let Job {
                state: JobState::Active(pid),
                ..
            } = job
            {
                async_pids.push(AsyncPidWatch::new(*pid)?);
            }
        }
        Ok(async_pids)
    }

    async fn update_active_jobs(
        &self,
        ps: &HashMap<Pid, Process>,
        jobs: &mut HashMap<String, Job>,
        jobs_data: &JobsData,
    ) {
        for (name, job) in jobs.iter_mut() {
            if let Job {
                state: JobState::Active(job_pid),
                ..
            } = job
            {
                if !ps.keys().any(|pid| pid == job_pid) {
                    job.state = self
                        .handle_stopped_job(name, &job.config.restart, jobs_data)
                        .await;
                }
            }
        }
    }

    async fn check_inactive_jobs(&self, jobs: &mut HashMap<String, Job>, jobs_data: &JobsData) {
        let deps = jobs.clone();
        for (name, job) in jobs.iter_mut() {
            if let Job {
                state: JobState::Inactive(status),
                config: JobConfig { needs, .. },
            } = job
            {
                if *status == JobStatus::Pending {
                    match deps_finished(name, &deps, needs) {
                        Ok(true) => job.state = self.start_job(name, jobs_data).await,
                        Ok(false) => {}
                        Err(err) => {
                            *status = JobStatus::Finished {
                                exit_code: None,
                                message: err.to_string(),
                            };
                            if let Err(err) = jobs_data.save_status(status, name) {
                                // if we can't save save status for some reason, just log
                                error!("failed to save failed job {name} status: {err}");
                            }
                        }
                    }
                }
            }
        }
    }

    async fn handle_stopped_job(
        &self,
        name: &str,
        restart_policy: &RestartPolicy,
        jobs_data: &JobsData,
    ) -> JobState {
        let status = jobs_data.load_status(name).unwrap_or_else(|err| {
            let message =
                format!("can't load job '{name}' status from file after it stopped, with: {err}");
            error!(message);
            match restart_policy {
                RestartPolicy::Never => JobStatus::Finished {
                    exit_code: None,
                    message,
                },
                _ => JobStatus::Running,
            }
        });
        if let JobStatus::Finished { .. } = status {
            JobState::Inactive(status)
        } else {
            // job process ended, but job was not finished - try restart
            self.start_job(name, jobs_data).await
        }
    }

    async fn start_job(&self, name: &str, jobs_data: &JobsData) -> JobState {
        jobs_data.clear_status(name);
        match self.start_job_runner(name).await {
            Ok(pid) => JobState::Active(pid),
            Err(err) => {
                let status = JobStatus::Finished {
                    exit_code: None,
                    message: format!("failed to start job {name}: {err}"),
                };
                match jobs_data.save_status(&status, name) {
                    Ok(()) => JobState::Inactive(status),
                    Err(_) => {
                        error!("failed to save failed job {name} status: {err}");
                        JobState::Inactive(JobStatus::Pending)
                    }
                }
            }
        }
    }

    async fn start_job_runner(&self, name: &str) -> Result<Pid> {
        // make sure job runner binary is not currently updated
        let lock = self.job_runner_lock.read().await;
        if lock.is_some() {
            let child = tokio::process::Command::new(&self.job_runner_bin_path)
                .args([&name])
                .spawn()?;
            // it is save to unwrap() here, id() returns None only if
            // "the child has been polled to completion"
            Ok(Pid::from_u32(child.id().unwrap()))
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
        match &deps
            .get(needed_name)
            .with_context(|| format!("job '{name}' needs '{needed_name}', but it is not defined"))?
            .state
        {
            JobState::Inactive(JobStatus::Finished {
                exit_code: Some(0), ..
            }) => {}
            JobState::Inactive(JobStatus::Finished { exit_code, message }) => bail!(
                "job '{name}' needs '{needed_name}', but it failed with {exit_code:?} - {message}"
            ),
            JobState::Inactive(JobStatus::Stopped) => {
                bail!("job '{name}' needs '{needed_name}', but it was stopped")
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}
