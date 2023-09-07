use crate::utils::gracefully_terminate_process;
/// Jobs Manager consists of two parts:
/// .1 Client - allow asynchronous operations on jobs: start, stop and get status
/// .2 Monitor - background worker that monitor job runners and take proper actions when some job runner ends
/// It also start/stop requested jobs.
use crate::{
    async_pid_watch::AsyncPidWatch,
    babel_service::JobRunnerLock,
    jobs,
    jobs::{Job, JobState, Jobs, JobsData, JobsRegistry, CONFIG_SUBDIR, STATUS_SUBDIR},
    utils::find_processes,
};
use async_trait::async_trait;
use babel_api::engine::{
    JobConfig, JobProgress, JobStatus, RestartPolicy, DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS,
};
use bv_utils::run_flag::RunFlag;
use eyre::{bail, Context, ContextCompat, Report, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashMap, fs, fs::read_dir, path::Path, sync::Arc, time::Duration};
use sysinfo::{Pid, PidExt, Process, System, SystemExt};
use tokio::{
    select,
    sync::{watch, Mutex},
};
use tracing::{debug, error, info, warn};

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum JobsManagerState {
    NotReady,
    Ready,
    Shutdown,
}

pub fn create(
    jobs_dir: &Path,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: &Path,
    state: JobsManagerState,
) -> Result<(Client, Manager)> {
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
    let (jobs_manager_state_tx, jobs_manager_state_rx) = watch::channel(state);

    Ok((
        Client {
            jobs_registry: jobs_registry.clone(),
            job_added_tx,
            jobs_manager_state_tx,
        },
        Manager {
            jobs_registry,
            job_runner_lock,
            job_runner_bin_path: job_runner_bin_path.to_string_lossy().to_string(),
            job_added_rx,
            jobs_manager_state_rx,
        },
    ))
}

fn load_jobs(jobs_dir: &Path, job_runner_bin_path: &Path) -> Result<Jobs> {
    info!("Loading jobs list from {} ...", jobs_dir.display());
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
                    let state = if let Some((pid, _)) = find_processes(
                        &job_runner_bin_path.to_string_lossy(),
                        vec![name.to_string()],
                        ps,
                    )
                    .next()
                    {
                        info!("{name} - Active(PID: {pid})");
                        JobState::Active(*pid)
                    } else {
                        let status = jobs_data.load_status(&name).unwrap_or(JobStatus::Pending);
                        info!("{name} - Inactive(status: {status:?})");
                        JobState::Inactive(status)
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
    async fn startup(&self) -> Result<()>;
    async fn get_active_jobs_shutdown_timeout(&self) -> Duration;
    async fn shutdown(&self) -> Result<()>;
    async fn list(&self) -> Result<Vec<(String, JobStatus)>>;
    async fn start(&self, name: &str, config: JobConfig) -> Result<()>;
    async fn stop(&self, name: &str) -> Result<()>;
    async fn status(&self, name: &str) -> Result<JobStatus>;
    async fn progress(&self, name: &str) -> Result<JobProgress>;
}

pub struct Client {
    jobs_registry: JobsRegistry,
    job_added_tx: watch::Sender<()>,
    jobs_manager_state_tx: watch::Sender<JobsManagerState>,
}

#[async_trait]
impl JobsManagerClient for Client {
    async fn startup(&self) -> Result<()> {
        info!("Startup jobs manager - set state to 'Ready'");
        self.jobs_manager_state_tx.send(JobsManagerState::Ready)?;
        Ok(())
    }

    async fn get_active_jobs_shutdown_timeout(&self) -> Duration {
        let (jobs, _) = &*self.jobs_registry.lock().await;
        let total_timeout = jobs.iter().fold(Duration::default(), |acc, (_, job)| {
            if let JobState::Active(_) = job.state {
                acc + Duration::from_secs(
                    job.config
                        .shutdown_timeout_secs
                        .unwrap_or(DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS),
                )
            } else {
                acc
            }
        });
        debug!(
            "Get active jobs total timeout: {}s",
            total_timeout.as_secs()
        );
        total_timeout
    }

    async fn shutdown(&self) -> Result<()> {
        info!("Shutdown jobs manager - set state to 'Shutdown'");
        // ignore send error since jobs_manager may be already stopped
        let _ = self.jobs_manager_state_tx.send(JobsManagerState::Shutdown);
        let (jobs, _) = &mut *self.jobs_registry.lock().await;
        for (name, job) in jobs {
            if let JobState::Active(pid) = &mut job.state {
                terminate_job(name, pid, &job.config)?;
                // job_runner process has been stopped, but job should be restarted on next jobs manager startup
                job.state = JobState::Inactive(JobStatus::Running);
            }
        }
        Ok(())
    }

    async fn list(&self) -> Result<Vec<(String, JobStatus)>> {
        let (jobs, _) = &*self.jobs_registry.lock().await;
        let res = jobs
            .iter()
            .map(|(name, job)| {
                (
                    name.clone(),
                    match &job.state {
                        JobState::Active(_) => JobStatus::Running,
                        JobState::Inactive(status) => status.clone(),
                    },
                )
            })
            .collect();
        Ok(res)
    }

    async fn start(&self, name: &str, config: JobConfig) -> Result<()> {
        info!("Requested '{name} job to start'");
        debug!("Requested '{name} job to start with {config:?}");
        let (jobs, jobs_data) = &mut *self.jobs_registry.lock().await;
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
            format!("failed to start job '{name}', can't save job config to file")
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
        info!("Requested '{name} job to stop'");
        let (jobs, jobs_data) = &mut *self.jobs_registry.lock().await;
        if let Some(job) = jobs.get_mut(name) {
            match &mut job.state {
                JobState::Active(pid) => {
                    terminate_job(name, pid, &job.config)?;
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
        let (jobs, _) = &*self.jobs_registry.lock().await;
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

    async fn progress(&self, name: &str) -> Result<JobProgress> {
        let (jobs, jobs_data) = &*self.jobs_registry.lock().await;
        let progress = if jobs.contains_key(name) {
            jobs_data.load_progress(name)
        } else {
            bail!("unknown progress, job '{name}' not found")
        };
        Ok(progress)
    }
}

fn terminate_job(name: &str, pid: &Pid, config: &JobConfig) -> Result<()> {
    let shutdown_timeout = Duration::from_secs(
        config
            .shutdown_timeout_secs
            .unwrap_or(DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS),
    );
    info!(
        "Terminate job '{name}' with timeout {}s",
        shutdown_timeout.as_secs()
    );
    if !gracefully_terminate_process(pid, shutdown_timeout) {
        bail!("Failed to terminate job_runner for '{name}' job (pid {pid}), timeout expired!");
    }
    Ok(())
}

pub struct Manager {
    jobs_registry: JobsRegistry,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: String,
    job_added_rx: watch::Receiver<()>,
    jobs_manager_state_rx: watch::Receiver<JobsManagerState>,
}

impl Manager {
    pub async fn run(mut self, mut run: RunFlag) {
        debug!(
            "Started Jobs Manager in state {:?}",
            self.jobs_manager_state_rx.borrow()
        );
        let mut sys = System::new();
        // do not start any job until jobs manager is ready
        while run.load() && *self.jobs_manager_state_rx.borrow() == JobsManagerState::NotReady {
            run.select(self.jobs_manager_state_rx.changed()).await;
        }
        while run.load() && *self.jobs_manager_state_rx.borrow() == JobsManagerState::Ready {
            sys.refresh_processes();
            if let Ok(async_pids) = self.update_jobs(sys.processes()).await {
                if async_pids.is_empty() {
                    // no jobs :( - just wait for job to be added
                    select!(
                        _ = self.job_added_rx.changed() => {}
                        _ = self.jobs_manager_state_rx.changed() => {}
                        _ = run.wait() => {}
                    );
                } else {
                    let mut futures: FuturesUnordered<_> =
                        async_pids.iter().map(|a| a.watch()).collect();
                    select!(
                        _ = futures.next() => {}
                        _ = self.job_added_rx.changed() => {}
                        _ = self.jobs_manager_state_rx.changed() => {}
                        _ = run.wait() => {}
                    );
                }
            } // refresh process and update_jobs again in case of error
        }
    }

    async fn update_jobs(&mut self, ps: &HashMap<Pid, Process>) -> Result<Vec<AsyncPidWatch>> {
        let (jobs, jobs_data) = &mut *self.jobs_registry.lock().await;
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
                    if let Some(needs) = needs {
                        match deps_finished(name, &deps, needs) {
                            Ok(true) => {
                                info!("all '{name}' job dependencies finished");
                                job.state = self.start_job(name, jobs_data).await;
                            }
                            Ok(false) => {}
                            Err(err) => {
                                warn!("{err}");
                                *status = JobStatus::Finished {
                                    exit_code: None,
                                    message: err.to_string(),
                                };
                                if let Err(err) = jobs_data.save_status(status, name) {
                                    // if we can't save save status for some reason, just log
                                    error!("failed to save failed job '{name}' status: {err}");
                                }
                            }
                        }
                    } else {
                        job.state = self.start_job(name, jobs_data).await
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
            info!("job '{name}' finished with {status:?}");
            JobState::Inactive(status)
        } else {
            warn!("job '{name}' process ended, but job was not finished - try restart");
            self.start_job(name, jobs_data).await
        }
    }

    async fn start_job(&self, name: &str, jobs_data: &JobsData) -> JobState {
        jobs_data.clear_status(name);
        match self.start_job_runner(name).await {
            Ok(pid) => {
                info!("started job '{name}' with PID {pid}");
                JobState::Active(pid)
            }
            Err(err) => {
                let message = format!("failed to start job '{name}': {err}");
                warn!(message);
                let status = JobStatus::Finished {
                    exit_code: None,
                    message,
                };
                match jobs_data.save_status(&status, name) {
                    Ok(()) => JobState::Inactive(status),
                    Err(_) => {
                        error!("failed to save failed job '{name}' status: {err}");
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
    needs: &[String],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::kill_process_by_name;
    use assert_fs::TempDir;
    use babel_api::engine::{JobType, RestartConfig};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::process::Command;
    use tokio::sync::RwLock;
    use tokio::task::JoinHandle;

    struct TestEnv {
        ctrl_file: PathBuf,
        jobs_dir: PathBuf,
        jobs_config_dir: PathBuf,
        jobs_status_dir: PathBuf,
        test_job_runner_path: PathBuf,
        run: RunFlag,
        client: Client,
        monitor: Option<Manager>,
    }

    impl TestEnv {
        fn setup() -> Result<Self> {
            let tmp_root = TempDir::new()?.to_path_buf();
            let ctrl_file = tmp_root.join("job_runner_started");
            let jobs_dir = tmp_root.join("jobs");
            let jobs_config_dir = jobs_dir.join(CONFIG_SUBDIR);
            let jobs_status_dir = jobs_dir.join(STATUS_SUBDIR);
            let test_job_runner_path = tmp_root.join("test_job_runner");
            let run = RunFlag::default();
            let (client, monitor) = create(
                &jobs_dir,
                Arc::new(RwLock::new(Some(0))),
                &test_job_runner_path,
                JobsManagerState::Ready,
            )?;
            Ok(Self {
                ctrl_file,
                jobs_dir,
                jobs_config_dir,
                jobs_status_dir,
                test_job_runner_path,
                run,
                client,
                monitor: Some(monitor),
            })
        }

        fn spawn_monitor(&mut self) -> JoinHandle<()> {
            tokio::spawn(self.monitor.take().unwrap().run(self.run.clone()))
        }

        fn create_infinite_job_runner(&self) -> Result<()> {
            let mut job_runner = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o770)
                .open(&self.test_job_runner_path)?;
            writeln!(job_runner, "#!/bin/sh")?;
            writeln!(job_runner, "touch {}", self.ctrl_file.to_string_lossy())?;
            writeln!(job_runner, "sleep infinity")?;
            Ok(())
        }

        async fn wait_for_job_runner(&self) -> Result<()> {
            // asynchronously wait for dummy job_runner to start
            tokio::time::timeout(Duration::from_secs(1), async {
                while !self.ctrl_file.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            Ok(())
        }

        fn kill_job(&self, name: &str) {
            kill_process_by_name(
                &self.test_job_runner_path.to_owned().to_string_lossy(),
                vec![name.to_string()],
            )
        }
    }

    fn dummy_job_config() -> JobConfig {
        JobConfig {
            job_type: JobType::RunSh("".to_string()),
            restart: RestartPolicy::Never,
            shutdown_timeout_secs: None,
            needs: None,
        }
    }

    #[tokio::test]
    async fn test_client_start() -> Result<()> {
        let test_env = TestEnv::setup()?;
        let _ = test_env.client.status("missing_job").await.unwrap_err();

        // start OK
        let status_path = test_env.jobs_status_dir.join("test_job.status");
        {
            // create dummy status file to make sure it is removed after start
            let mut status_file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&status_path)?;
            writeln!(status_file, "empty")?;
        }
        let config = dummy_job_config();
        test_env.client.start("test_job", config.clone()).await?;
        assert!(!status_path.exists());
        let saved_config = jobs::load_config(&test_env.jobs_config_dir.join("test_job.cfg"))?;
        assert_eq!(config, saved_config);
        assert!(test_env
            .monitor
            .unwrap()
            .job_added_rx
            .has_changed()
            .unwrap());
        assert_eq!(
            JobStatus::Pending,
            test_env.client.status("test_job").await?
        );

        // start failed
        test_env
            .client
            .jobs_registry
            .lock()
            .await
            .0
            .get_mut("test_job")
            .unwrap()
            .state = JobState::Active(Pid::from_u32(0));
        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_job").await?
        );
        let _ = test_env
            .client
            .start(
                "test_job",
                JobConfig {
                    job_type: JobType::RunSh("different".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    needs: Some(vec![]),
                },
            )
            .await
            .unwrap_err();
        Ok(())
    }

    #[tokio::test]
    async fn test_client_stop() -> Result<()> {
        let test_env = TestEnv::setup()?;

        // stop missing
        let _ = test_env.client.stop("missing_job").await.unwrap_err();

        // stop inactive
        test_env.client.jobs_registry.lock().await.0.insert(
            "test_job".to_owned(),
            Job {
                state: JobState::Inactive(JobStatus::Pending),
                config: dummy_job_config(),
            },
        );
        test_env.client.stop("test_job").await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.status("test_job").await?
        );
        let saved_status = jobs::load_status(&test_env.jobs_status_dir.join("test_job.status"))?;
        assert_eq!(JobStatus::Stopped, saved_status);

        // stop active
        test_env.client.jobs_registry.lock().await.0.insert(
            "test_active_job".to_owned(),
            Job {
                state: JobState::Active(Pid::from_u32(0)),
                config: dummy_job_config(),
            },
        );
        test_env.client.stop("test_active_job").await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.status("test_active_job").await?
        );
        let saved_status =
            jobs::load_status(&test_env.jobs_status_dir.join("test_active_job.status"))?;
        assert_eq!(JobStatus::Stopped, saved_status);

        Ok(())
    }

    #[tokio::test]
    async fn test_load_jobs() -> Result<()> {
        let test_env = TestEnv::setup()?;

        // no jobs
        let (jobs, jobs_data) = load_jobs(&test_env.jobs_dir, &test_env.test_job_runner_path)?;
        assert!(jobs.is_empty());

        // load active and inactive jobs
        let config = dummy_job_config();
        jobs_data.save_config(&config, "pending_job")?;
        jobs_data.save_config(&config, "finished_job")?;
        jobs_data.save_status(
            &JobStatus::Finished {
                exit_code: Some(1),
                message: "job message".to_string(),
            },
            "finished_job",
        )?;
        jobs_data.save_config(&config, "active_job")?;
        test_env.create_infinite_job_runner()?;
        let mut job = Command::new(&test_env.test_job_runner_path)
            .args(["active_job"])
            .spawn()?;
        test_env.wait_for_job_runner().await?;
        let invalid_config_path = test_env.jobs_config_dir.join("invalid.cfg");
        {
            // create invalid config file to make sure it won't crash load and is removed after
            let mut invalid_config = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&invalid_config_path)?;
            writeln!(invalid_config, "gibberish")?;
        }

        let (jobs, _) = load_jobs(&test_env.jobs_dir, &test_env.test_job_runner_path)?;

        assert_eq!(
            Job {
                state: JobState::Inactive(JobStatus::Pending),
                config: config.clone(),
            },
            *jobs.get("pending_job").unwrap()
        );
        assert_eq!(
            Job {
                state: JobState::Inactive(JobStatus::Finished {
                    exit_code: Some(1),
                    message: "job message".to_string(),
                }),
                config: config.clone(),
            },
            *jobs.get("finished_job").unwrap()
        );
        assert_eq!(
            Job {
                state: JobState::Active(Pid::from_u32(job.id().unwrap())),
                config: config.clone(),
            },
            *jobs.get("active_job").unwrap()
        );
        job.kill().await?;
        assert!(!invalid_config_path.exists());

        // invalid dir
        fs::remove_dir_all(&test_env.jobs_dir)?;
        let _ = load_jobs(&test_env.jobs_dir, &test_env.test_job_runner_path).unwrap_err();
        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_job_shutdown() -> Result<()> {
        let mut test_env = TestEnv::setup()?;
        test_env
            .client
            .start("test_job", dummy_job_config())
            .await?;
        test_env.create_infinite_job_runner()?;

        test_env
            .client
            .jobs_manager_state_tx
            .send(JobsManagerState::NotReady)?;
        let monitor_handle = test_env.spawn_monitor();

        assert_eq!(
            JobStatus::Pending,
            test_env.client.status("test_job").await?
        );

        let _ = test_env.wait_for_job_runner().await.unwrap_err();
        test_env.client.startup().await?;
        test_env.wait_for_job_runner().await?;

        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_job").await?
        );
        test_env.client.shutdown().await?;

        monitor_handle.await?;

        let mut sys = System::new();
        sys.refresh_processes();
        let ps = sys.processes();
        assert!(find_processes(
            &test_env.test_job_runner_path.to_string_lossy(),
            vec!["test_job".to_string()],
            ps
        )
        .next()
        .is_none());
        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_job").await?
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_missing_job_runner() -> Result<()> {
        let mut test_env = TestEnv::setup()?;
        test_env
            .client
            .start("test_job", dummy_job_config())
            .await?;

        let monitor_handle = test_env.spawn_monitor();

        while JobStatus::Pending == test_env.client.status("test_job").await? {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: "failed to start job 'test_job': No such file or directory (os error 2)"
                    .to_string()
            },
            test_env.client.status("test_job").await?
        );

        test_env.run.stop();
        monitor_handle.await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_dependencies() -> Result<()> {
        let mut test_env = TestEnv::setup()?;
        test_env.create_infinite_job_runner()?;
        test_env
            .client
            .start(
                "test_invalid_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    needs: Some(vec!["invalid_dependency".to_string()]),
                },
            )
            .await?;
        test_env
            .client
            .start("test_job", dummy_job_config())
            .await?;
        test_env
            .client
            .start(
                "test_pending_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    needs: Some(vec!["test_job".to_string()]),
                },
            )
            .await?;

        let monitor_handle = test_env.spawn_monitor();
        test_env.wait_for_job_runner().await?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: "job 'test_invalid_job' needs 'invalid_dependency', but it is not defined"
                    .to_string()
            },
            test_env.client.status("test_invalid_job").await?
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_job").await?
        );
        assert_eq!(
            JobStatus::Pending,
            test_env.client.status("test_pending_job").await?
        );

        jobs::save_status(
            &JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string(),
            },
            "test_job",
            &test_env.jobs_status_dir,
        )?;
        fs::remove_file(&test_env.ctrl_file)?;
        test_env.kill_job("test_job");

        test_env.wait_for_job_runner().await?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string(),
            },
            test_env.client.status("test_job").await?
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_pending_job").await?
        );

        test_env.run.stop();
        monitor_handle.await?;

        test_env.kill_job("test_pending_job");

        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_failed_dependency() -> Result<()> {
        let mut test_env = TestEnv::setup()?;
        test_env.create_infinite_job_runner()?;
        test_env
            .client
            .start("test_job", dummy_job_config())
            .await?;
        test_env
            .client
            .start(
                "test_pending_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    needs: Some(vec!["failed_job".to_string()]),
                },
            )
            .await?;
        // emulate failed job
        jobs::save_status(
            &JobStatus::Finished {
                exit_code: Some(1),
                message: "some job error".to_string(),
            },
            "failed_job",
            &test_env.jobs_status_dir,
        )?;
        test_env.client.jobs_registry.lock().await.0.insert(
            "failed_job".to_owned(),
            Job {
                state: JobState::Active(Pid::from_u32(0)),
                config: dummy_job_config(),
            },
        );

        let monitor_handle = test_env.spawn_monitor();
        test_env.wait_for_job_runner().await?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: "job 'test_pending_job' needs 'failed_job', but it failed with Some(1) - some job error"
                    .to_string()
            },
            test_env.client.status("test_pending_job").await?
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_job").await?
        );

        test_env.run.stop();
        monitor_handle.await?;

        test_env.kill_job("test_job");

        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_restart_crashed_job() -> Result<()> {
        let mut test_env = TestEnv::setup()?;
        test_env.create_infinite_job_runner()?;
        let monitor_handle = test_env.spawn_monitor();

        test_env
            .client
            .start("test_job", dummy_job_config())
            .await?;
        test_env.wait_for_job_runner().await?;

        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_job").await?
        );

        fs::remove_file(&test_env.ctrl_file)?;
        test_env.kill_job("test_job");
        test_env
            .client
            .start(
                "test_restarting_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Always(RestartConfig {
                        backoff_timeout_ms: 0,
                        backoff_base_ms: 0,
                        max_retries: None,
                    }),
                    shutdown_timeout_secs: None,
                    needs: None,
                },
            )
            .await?;
        test_env.wait_for_job_runner().await?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: format!("can't load job 'test_job' status from file after it stopped, with: Failed to read job status file `{}`",
                                 test_env.jobs_status_dir.join("test_job.status").display()),
            },
            test_env.client.status("test_job").await?
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_restarting_job").await?
        );

        fs::remove_file(&test_env.ctrl_file)?;
        test_env.kill_job("test_restarting_job");
        test_env.wait_for_job_runner().await?;

        assert_eq!(
            JobStatus::Running,
            test_env.client.status("test_restarting_job").await?
        );

        test_env.run.stop();
        monitor_handle.await?;

        test_env.kill_job("test_restarting_job");

        Ok(())
    }
}
