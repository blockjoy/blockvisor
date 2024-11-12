/// Jobs Manager consists of three parts:
/// .1 Client - allow asynchronously request operations on jobs (like start, stop and get info)
///             and other interactions with `Manager`
/// .2 Monitor - service listening `job_runner`s for job related logs and restart info
/// .3 Manager - background worker that maintains job runners and take proper actions when some job runner ends
use crate::{
    async_pid_watch::AsyncPidWatch,
    babel_service::JobRunnerLock,
    jobs::{self, Job, JobState, JobsContext, JobsRegistry},
    pal::BabelEngineConnector,
};
use async_trait::async_trait;
use babel_api::engine::{
    JobConfig, JobInfo, JobStatus, JobsInfo, NodeEnv, PosixSignal, RestartPolicy,
    DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS,
};
use bv_utils::{
    run_flag::RunFlag,
    system::{find_processes, gracefully_terminate_process, kill_all_processes},
    with_retry,
};
use eyre::{bail, Context, ContextCompat, Report, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashMap, path::Path, path::PathBuf, sync::Arc, time::Duration};
use sysinfo::{Pid, PidExt, Process, System, SystemExt};
use tokio::{
    fs, select,
    sync::{mpsc, watch, Mutex},
};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

pub const MAX_JOBS: usize = 16;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum JobsManagerState {
    NotReady,
    Ready,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
enum JobReport {
    PushLog { name: String, message: String },
    RegisterRestart { name: String },
}

pub async fn create<C: BabelEngineConnector>(
    mut jobs_context: JobsContext<C>,
    jobs_dir: &Path,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: &Path,
    state: JobsManagerState,
) -> Result<(Client<C>, Monitor, Manager<C>)> {
    if !jobs_dir.exists() {
        fs::create_dir_all(jobs_dir).await?;
    }
    if state == JobsManagerState::Ready {
        load_jobs(&mut jobs_context, &job_runner_bin_path.to_string_lossy()).await?;
    }
    let jobs_registry = Arc::new(Mutex::new(jobs_context));
    let (job_added_tx, job_added_rx) = watch::channel(());
    let (jobs_manager_state_tx, jobs_manager_state_rx) = watch::channel(state);
    let (jobs_monitor_tx, jobs_monitor_rx) = mpsc::channel(256);
    Ok((
        Client {
            jobs_registry: jobs_registry.clone(),
            job_runner_bin_path: job_runner_bin_path.to_string_lossy().to_string(),
            jobs_dir: jobs_dir.to_path_buf(),
            job_added_tx,
            jobs_manager_state_tx,
        },
        Monitor { jobs_monitor_tx },
        Manager {
            jobs_registry,
            job_runner_lock,
            job_runner_bin_path: job_runner_bin_path.to_string_lossy().to_string(),
            job_added_rx,
            jobs_manager_state_rx,
            jobs_monitor_rx,
        },
    ))
}

async fn load_jobs(
    jobs_context: &mut JobsContext<impl BabelEngineConnector>,
    job_runner_bin_path: &str,
) -> Result<()> {
    info!(
        "Loading jobs list from {} ...",
        jobs_context.jobs_dir.display()
    );
    // TODO MJR load jobs also from legacy directories
    // TODO MJR keep logs config, status and progress paths in Job structure
    let mut dir = fs::read_dir(&jobs_context.jobs_dir)
        .await
        .with_context(|| {
            format!(
                "failed to read jobs from dir {}",
                jobs_context.jobs_dir.display()
            )
        })?;
    let job_name = |path: &Path| {
        if path.is_dir() {
            let config_path = path.join(jobs::CONFIG_FILENAME);
            if config_path.exists() {
                path.components()
                    .last()
                    .map(|name| name.as_os_str().to_string_lossy().to_string())
            } else {
                None
            }
        } else {
            None
        }
    };
    let mut sys = System::new();
    sys.refresh_processes();
    let ps = sys.processes();
    while let Some(entry) = dir
        .next_entry()
        .await
        .with_context(|| "failed to read jobs registry entry")?
    {
        let job_dir = entry.path();
        let Some(name) = job_name(&job_dir) else {
            continue;
        };
        match jobs::load_config(&job_dir) {
            Ok(config) => {
                let state = if let Some((pid, _)) =
                    find_processes(job_runner_bin_path, &[&name], ps).next()
                {
                    info!("{name} - Active(PID: {pid})");
                    // TODO MJR let legacy node running, but add legacy metadata
                    JobState::Active(*pid)
                } else {
                    // TODO MJR job can be migrated to new version (move files)
                    let status = jobs::load_status(&job_dir).unwrap_or(JobStatus::Pending {
                        waiting_for: config.waiting_for(),
                    });
                    info!("{name} - Inactive(status: {status:?})");
                    JobState::Inactive(status)
                };
                jobs_context
                    .jobs
                    .insert(name, Job::new(job_dir, config, state));
            }
            Err(err) => {
                // invalid job config file log error, remove invalid file and go to next one
                let err_msg = format!(
                    "invalid job '{}' config file in {}, load failed with: {:#}",
                    name,
                    job_dir.display(),
                    err
                );
                error!(err_msg);
                let mut client = jobs_context.connector.connect();
                let _ = with_retry!(client.bv_error(err_msg.clone()));

                let _ = fs::remove_file(job_dir.join(jobs::CONFIG_FILENAME)).await;
            }
        }
    }
    Ok(())
}

#[async_trait]
pub trait JobsManagerClient {
    async fn startup(&self, node_env: NodeEnv) -> Result<()>;
    async fn get_active_jobs_shutdown_timeout(&self) -> Duration;
    async fn shutdown(&self, force: bool) -> Result<()>;
    async fn list(&self) -> Result<JobsInfo>;
    async fn create(&self, name: &str, config: JobConfig) -> Result<()>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn get_job_shutdown_timeout(&self, name: &str) -> Duration;
    async fn stop(&self, name: &str) -> Result<()>;
    async fn skip(&self, name: &str) -> Result<()>;
    async fn cleanup(&self, name: &str) -> Result<()>;
    async fn info(&self, name: &str) -> Result<JobInfo>;
}

pub struct Client<C> {
    jobs_registry: JobsRegistry<C>,
    job_runner_bin_path: String,
    jobs_dir: PathBuf,
    job_added_tx: watch::Sender<()>,
    jobs_manager_state_tx: watch::Sender<JobsManagerState>,
}

#[async_trait]
impl<C: BabelEngineConnector + Send> JobsManagerClient for Client<C> {
    async fn startup(&self, node_env: NodeEnv) -> Result<()> {
        info!("Startup jobs manager - load jobs and set state to 'Ready'");
        let mut jobs_context = self.jobs_registry.lock().await;
        jobs_context.node_env = Some(node_env);
        load_jobs(&mut *jobs_context, &self.job_runner_bin_path).await?;
        self.jobs_manager_state_tx.send(JobsManagerState::Ready)?;
        Ok(())
    }

    async fn get_active_jobs_shutdown_timeout(&self) -> Duration {
        let jobs = &self.jobs_registry.lock().await.jobs;
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

    async fn shutdown(&self, force: bool) -> Result<()> {
        info!("Shutdown jobs manager - set state to 'Shutdown'");
        // ignore send error since jobs_manager may be already stopped
        let _ = self.jobs_manager_state_tx.send(JobsManagerState::Shutdown);
        let jobs = &mut self.jobs_registry.lock().await.jobs;
        for (name, job) in jobs {
            if let JobState::Active(pid) = &mut job.state {
                if force {
                    kill_all_processes(
                        &self.job_runner_bin_path,
                        &[name],
                        Duration::from_secs(
                            job.config
                                .shutdown_timeout_secs
                                .unwrap_or(DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS),
                        ),
                        PosixSignal::SIGTERM,
                    );
                } else {
                    terminate_job(name, *pid, &job.config).await?;
                }
                // job_runner process has been stopped, but job should be restarted on next jobs manager startup
                job.state = JobState::Inactive(JobStatus::Running);
            }
        }
        Ok(())
    }

    async fn list(&self) -> Result<JobsInfo> {
        let jobs_context = &mut *self.jobs_registry.lock().await;
        let res = jobs_context
            .jobs
            .iter_mut()
            .map(|(name, job)| {
                job.update();
                (name.clone(), build_job_info(job))
            })
            .collect();
        Ok(res)
    }

    async fn create(&self, name: &str, config: JobConfig) -> Result<()> {
        info!("Requested '{name}' job to create: {config:?}",);
        let mut jobs_context = self.jobs_registry.lock().await;

        if let Some(Job { state, .. }) = jobs_context.jobs.get(name) {
            if let JobState::Active(_) = state {
                bail!("can't create job '{name}' while it is already running")
            }
        } else if jobs_context.jobs.len() >= MAX_JOBS {
            bail!("Exceeded max number of supported jobs: {MAX_JOBS}");
        }
        let job_dir = self.jobs_dir.join(name);
        if !job_dir.exists() {
            fs::create_dir_all(&job_dir)
                .await
                .with_context(|| format!("failed to create job dir {}", job_dir.display()))?;
        }
        let job = Job::new(job_dir, config, JobState::Inactive(JobStatus::Stopped));
        job.save_status()?;
        job.save_config().with_context(|| {
            format!("failed to create job '{name}', can't save job config to file")
        })?;
        jobs_context.jobs.insert(name.to_string(), job);
        let _ = self.job_added_tx.send(());
        Ok(())
    }

    async fn start(&self, name: &str) -> Result<()> {
        info!("Requested '{name}' job to start");
        let mut jobs_context = self.jobs_registry.lock().await;
        let jobs = &mut jobs_context.jobs;
        if let Some(job) = jobs.get_mut(name) {
            match &mut job.state {
                JobState::Active(_) => return Ok(()),
                JobState::Inactive(status) => {
                    *status = JobStatus::Pending {
                        waiting_for: job.config.waiting_for(),
                    };
                    job.save_status()?;
                }
            }
        } else {
            bail!("can't start, job '{name}' not found")
        }
        let _ = self.job_added_tx.send(());
        Ok(())
    }

    async fn get_job_shutdown_timeout(&self, name: &str) -> Duration {
        self.jobs_registry
            .lock()
            .await
            .jobs
            .get(name)
            .and_then(|job| {
                if let JobState::Active(_) = &job.state {
                    Some(Duration::from_secs(
                        job.config
                            .shutdown_timeout_secs
                            .unwrap_or(DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS),
                    ))
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    async fn stop(&self, name: &str) -> Result<()> {
        info!("Requested '{name} job to stop'");
        let mut jobs_context = self.jobs_registry.lock().await;
        if let Some(job) = jobs_context.jobs.get_mut(name) {
            match &mut job.state {
                JobState::Active(pid) => {
                    terminate_job(name, *pid, &job.config).await?;
                    job.state = JobState::Inactive(JobStatus::Stopped);
                }
                JobState::Inactive(status) => {
                    *status = JobStatus::Stopped;
                }
            }
            job.save_status()?;
        } else {
            bail!("can't stop, job '{name}' not found")
        }
        Ok(())
    }

    async fn skip(&self, name: &str) -> Result<()> {
        info!("Requested '{name} job to stop'");
        let mut jobs_context = self.jobs_registry.lock().await;
        if let Some(job) = jobs_context.jobs.get_mut(name) {
            match &mut job.state {
                JobState::Active(pid) => {
                    terminate_job(name, *pid, &job.config).await?;
                    job.state = JobState::Inactive(JobStatus::Finished {
                        exit_code: Some(0),
                        message: "Skipped".to_string(),
                    });
                }
                JobState::Inactive(status) => {
                    *status = JobStatus::Finished {
                        exit_code: Some(0),
                        message: "Skipped".to_string(),
                    };
                }
            }
            job.save_status()?;
        } else {
            bail!("can't stop, job '{name}' not found")
        }
        Ok(())
    }

    async fn cleanup(&self, name: &str) -> Result<()> {
        info!("Requested '{name} job to cleanup'");
        let jobs_context = self.jobs_registry.lock().await;
        let Some(job) = jobs_context.jobs.get(name) else {
            bail!("can't cleanup, job '{name}' not found")
        };
        let Some(node_env) = &jobs_context.node_env else {
            bail!("can't cleanup, job '{name}', missing node_env")
        };
        if let JobState::Inactive(_) = job.state {
            job.cleanup(node_env)
        } else {
            bail!("can't cleanup active job '{name}'");
        }
    }

    async fn info(&self, name: &str) -> Result<JobInfo> {
        let jobs_context = &mut *self.jobs_registry.lock().await;
        let job = jobs_context
            .jobs
            .get_mut(name)
            .with_context(|| format!("unknown status, job '{name}' not found"))?;
        job.update();
        Ok(build_job_info(job))
    }
}

fn build_job_info(job: &Job) -> JobInfo {
    let restart_count = job.restart_stamps.len();
    let progress = job.load_progress();
    let logs = job.logs.iter().rev().map(|(_, log)| log.clone()).collect();
    JobInfo {
        status: if let Job {
            state: JobState::Inactive(status),
            ..
        } = job
        {
            status.clone()
        } else {
            JobStatus::Running
        },
        progress,
        restart_count,
        logs,
        upgrade_blocking: match &job.config.restart {
            RestartPolicy::Always(_) => false,
            RestartPolicy::OnFailure(config) if config.max_retries.is_none() => false,
            _ => true,
        },
    }
}

async fn terminate_job(name: &str, pid: Pid, config: &JobConfig) -> Result<()> {
    let shutdown_timeout = Duration::from_secs(
        config
            .shutdown_timeout_secs
            .unwrap_or(DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS),
    );
    info!(
        "Terminate job '{name}' with timeout {}s",
        shutdown_timeout.as_secs()
    );
    if !gracefully_terminate_process(pid, shutdown_timeout).await {
        bail!("Failed to terminate job_runner for '{name}' job (pid {pid}), timeout expired!");
    }
    Ok(())
}

pub struct Monitor {
    jobs_monitor_tx: mpsc::Sender<JobReport>,
}

#[tonic::async_trait]
impl babel_api::babel::jobs_monitor_server::JobsMonitor for Monitor {
    async fn push_log(&self, request: Request<(String, String)>) -> Result<Response<()>, Status> {
        let (name, message) = request.into_inner();
        self.jobs_monitor_tx
            .send(JobReport::PushLog { name, message })
            .await
            .map_err(|err| Status::internal(format!("{err:#}")))?;
        Ok(Response::new(()))
    }

    async fn register_restart(&self, request: Request<String>) -> Result<Response<()>, Status> {
        let name = request.into_inner();
        self.jobs_monitor_tx
            .send(JobReport::RegisterRestart { name })
            .await
            .map_err(|err| Status::internal(format!("{err:#}")))?;
        Ok(Response::new(()))
    }
}

pub struct Manager<C> {
    jobs_registry: JobsRegistry<C>,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: String,
    job_added_rx: watch::Receiver<()>,
    jobs_manager_state_rx: watch::Receiver<JobsManagerState>,
    jobs_monitor_rx: mpsc::Receiver<JobReport>,
}

impl<C: BabelEngineConnector> Manager<C> {
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
                    'monitor: loop {
                        select!(
                            report = self.jobs_monitor_rx.recv() => {
                                self.handle_job_report(report).await;
                                continue 'monitor
                            }
                            _ = futures.next() => {}
                            _ = self.job_added_rx.changed() => {}
                            _ = self.jobs_manager_state_rx.changed() => {}
                            _ = run.wait() => {}
                        );
                        break 'monitor;
                    }
                }
            } // refresh process and update_jobs again in case of error
        }
    }

    async fn handle_job_report(&mut self, report: Option<JobReport>) {
        match report {
            Some(JobReport::PushLog { name, message }) => {
                let jobs = &mut self.jobs_registry.lock().await.jobs;
                if let Some(job) = jobs.get_mut(&name) {
                    job.push_log(&message);
                }
            }
            Some(JobReport::RegisterRestart { name }) => {
                let jobs = &mut self.jobs_registry.lock().await.jobs;
                if let Some(job) = jobs.get_mut(&name) {
                    job.register_restart();
                }
            }
            None => {}
        }
    }

    async fn update_jobs(&mut self, ps: &HashMap<Pid, Process>) -> Result<Vec<AsyncPidWatch>> {
        let mut jobs_context = self.jobs_registry.lock().await;
        self.update_active_jobs(ps, &mut *jobs_context).await;
        self.check_inactive_jobs(&mut *jobs_context).await;

        let mut async_pids = vec![];
        for job in jobs_context.jobs.values() {
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
        jobs_context: &mut JobsContext<C>,
    ) {
        for (name, job) in jobs_context.jobs.iter_mut() {
            if let Job {
                state: JobState::Active(job_pid),
                ..
            } = job
            {
                if !ps.keys().any(|pid| pid == job_pid) {
                    self.handle_stopped_job(name, job, &jobs_context.connector)
                        .await;
                }
            }
        }
    }

    async fn check_inactive_jobs(&self, jobs_context: &mut JobsContext<C>) {
        let deps = jobs_context.jobs.clone();
        for (name, job) in jobs_context.jobs.iter_mut() {
            if let Job {
                state: JobState::Inactive(status),
                config: JobConfig {
                    needs, wait_for, ..
                },
                ..
            } = job
            {
                if matches!(status, JobStatus::Pending { .. }) {
                    match deps_finished(name, &deps, needs, wait_for) {
                        Ok(true) => {
                            if needs.is_some() || wait_for.is_some() {
                                info!("all '{name}' job dependencies finished");
                            }
                            self.start_job(name, job, &jobs_context.connector).await;
                        }
                        Ok(false) => {}
                        Err(err) => {
                            *status = JobStatus::Finished {
                                exit_code: None,
                                message: err.to_string(),
                            };
                            let save_result = job.save_status();
                            let message = err.to_string();
                            job.push_log(&message);
                            warn!(message);
                            if let Err(err) = save_result {
                                // if we can't save status for some reason, just log
                                let message =
                                    format!("failed to save failed job '{name}' status: {err:#}");
                                job.push_log(&message);
                                error!(message);
                                let mut client = jobs_context.connector.connect();
                                let _ = with_retry!(client.bv_error(message.clone()));
                            }
                        }
                    }
                }
            }
        }
    }

    async fn handle_stopped_job(&self, name: &str, job: &mut Job, connector: &C) {
        let status = match job.load_status() {
            Err(err) => {
                let message = format!(
                    "can't load job '{name}' status from file after it stopped, with: {err:#}"
                );
                job.push_log(&message);
                error!(message);
                let mut client = connector.connect();
                let _ = with_retry!(client.bv_error(message.clone()));
                match &job.config.restart {
                    RestartPolicy::Never => JobStatus::Finished {
                        exit_code: None,
                        message,
                    },
                    _ => JobStatus::Running,
                }
            }
            Ok(status) => status,
        };
        // TODO MJR migrate job if it was legacy job
        match status {
            JobStatus::Finished { .. } | JobStatus::Stopped => {
                info!("job '{name}' finished with {status:?}");
                job.state = JobState::Inactive(status)
            }
            _ => {
                job.register_restart();
                job.push_log("babel_job_runner process ended unexpectedly");
                warn!("job '{name}' process ended, but job was not finished - try restart");
                self.start_job(name, job, connector).await;
            }
        }
    }

    async fn start_job(&self, name: &str, job: &mut Job, connector: &C) {
        if let Err(err) = job.clear_status() {
            let message = format!("failed to clear job '{name}' status, before new run: {err:#}");
            job.push_log(&message);
            error!(message);
            let mut client = connector.connect();
            let _ = with_retry!(client.bv_error(message.clone()));
            job.state = JobState::Inactive(JobStatus::Pending {
                waiting_for: job.config.waiting_for(),
            });
        } else {
            match self.start_job_runner(name).await {
                Ok(pid) => {
                    info!("started job '{name}' with PID {pid}");
                    job.state = JobState::Active(pid);
                }
                Err(err) => {
                    let message = format!("failed to start job '{name}': {err:#}");
                    job.push_log(&message);
                    warn!(message);
                    job.state = JobState::Inactive(JobStatus::Finished {
                        exit_code: None,
                        message,
                    });
                    if job.save_status().is_err() {
                        let message = format!("failed to save failed job '{name}' status: {err:#}");
                        job.push_log(&message);
                        error!(message);
                        let mut client = connector.connect();
                        let _ = with_retry!(client.bv_error(message.clone()));
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
    needs: &Option<Vec<String>>,
    wait_for: &Option<Vec<String>>,
) -> Result<bool, Report> {
    if let Some(needs) = needs {
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
    }
    if let Some(wait_for) = wait_for {
        for wait_for_name in wait_for {
            let job = deps.get(wait_for_name).with_context(|| {
                format!("job '{name}' waits for '{wait_for_name}', but it is not defined")
            })?;
            match &job.state {
                JobState::Inactive(JobStatus::Finished { .. })
                | JobState::Inactive(JobStatus::Stopped) => {}
                _ => return Ok(false),
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;
    use assert_fs::TempDir;
    use babel_api::engine::{JobType, PosixSignal, RestartConfig};
    use bv_tests_utils::rpc::TestServer;
    use bv_utils::system::find_processes;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::sync::RwLock;
    use tokio::task::JoinHandle;

    struct TestEnv {
        tmp_dir: PathBuf,
        jobs_dir: PathBuf,
        test_job_runner_path: PathBuf,
        run: RunFlag,
        client: Client<utils::tests::DummyConnector>,
        manager: Option<Manager<utils::tests::DummyConnector>>,
        server: TestServer,
    }

    impl TestEnv {
        async fn setup() -> Result<Self> {
            let tmp_dir = TempDir::new()?.to_path_buf();
            fs::create_dir_all(&tmp_dir).await?;
            let jobs_dir = tmp_dir.join("jobs");
            let test_job_runner_path = tmp_dir.join("test_job_runner");
            let run = RunFlag::default();
            let mut engine_mock = utils::tests::MockBabelEngine::new();
            engine_mock
                .expect_bv_error()
                .returning(|_| Ok(Response::new(())));
            let server = bv_tests_utils::start_test_server!(
                &tmp_dir,
                babel_api::babel::babel_engine_server::BabelEngineServer::new(engine_mock)
            );
            let (client, _monitor, manager) = create(
                JobsContext {
                    jobs: Default::default(),
                    node_env: None,
                    jobs_dir: jobs_dir.clone(),
                    connector: utils::tests::DummyConnector {
                        tmp_dir: tmp_dir.clone(),
                    },
                },
                &jobs_dir,
                Arc::new(RwLock::new(Some(0))),
                &test_job_runner_path,
                JobsManagerState::Ready,
            )
            .await?;
            Ok(Self {
                tmp_dir: tmp_dir.clone(),
                jobs_dir,
                test_job_runner_path,
                run,
                client,
                manager: Some(manager),
                server,
            })
        }

        fn spawn_monitor(&mut self) -> JoinHandle<()> {
            tokio::spawn(self.manager.take().unwrap().run(self.run.clone()))
        }

        fn create_infinite_job_runner(&self) {
            utils::tests::create_dummy_bin(&self.test_job_runner_path, &self.tmp_dir, true);
        }

        async fn wait_for_job_runner(&self, name: &str) -> Result<()> {
            // asynchronously wait for dummy job_runner to start
            tokio::time::timeout(Duration::from_secs(3), async {
                while !self.tmp_dir.join(name).exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            Ok(())
        }

        fn kill_job(&self, name: &str) {
            bv_utils::system::kill_all_processes(
                &self.test_job_runner_path.to_owned().to_string_lossy(),
                &[name],
                Duration::from_secs(60),
                PosixSignal::SIGTERM,
            )
        }
    }

    fn dummy_job_config() -> JobConfig {
        JobConfig {
            job_type: JobType::RunSh("".to_string()),
            restart: RestartPolicy::Never,
            shutdown_timeout_secs: None,
            shutdown_signal: None,
            needs: None,
            wait_for: None,
            run_as: None,
            log_buffer_capacity_mb: None,
            log_timestamp: None,
        }
    }

    #[tokio::test]
    async fn test_client_create_max_jobs() -> Result<()> {
        let test_env = TestEnv::setup().await?;
        let _ = test_env.client.info("missing_job").await.unwrap_err();
        let test_job_dir = test_env.jobs_dir.join("test_job");
        fs::create_dir_all(&test_job_dir).await?;

        // start OK
        let status_path = test_job_dir.join(jobs::STATUS_FILENAME);
        {
            // create dummy status file to make sure it is removed after start
            let mut status_file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&status_path)
                .await?;
            status_file.write_all("empty\n".as_bytes()).await?;
        }
        let config = dummy_job_config();

        for i in 0..MAX_JOBS {
            test_env
                .client
                .create(&format!("test_job_{i}"), config.clone())
                .await?;
        }
        let _ = test_env
            .client
            .create("test_job_16", config.clone())
            .await
            .unwrap_err();
        test_env.server.assert().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_client_create_and_start() -> Result<()> {
        let test_env = TestEnv::setup().await?;
        let _ = test_env.client.info("missing_job").await.unwrap_err();
        let test_job_dir = test_env.jobs_dir.join("test_job");
        fs::create_dir_all(&test_job_dir).await?;

        // start OK
        let status_path = test_job_dir.join(jobs::STATUS_FILENAME);
        {
            // create dummy status file to make sure it is removed after start
            let mut status_file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&status_path)
                .await?;
            status_file.write_all("empty\n".as_bytes()).await?;
        }
        let config = dummy_job_config();
        test_env.client.create("test_job", config.clone()).await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.info("test_job").await?.status
        );
        test_env.client.start("test_job").await?;
        assert!(!status_path.exists());
        let saved_config = jobs::load_config(&test_job_dir)?;
        assert_eq!(config, saved_config);
        assert!(test_env
            .manager
            .unwrap()
            .job_added_rx
            .has_changed()
            .unwrap());
        assert_eq!(
            JobInfo {
                status: JobStatus::Pending {
                    waiting_for: vec![]
                },
                progress: Default::default(),
                restart_count: 0,
                logs: vec![],
                upgrade_blocking: true,
            },
            test_env.client.info("test_job").await?
        );

        // start failed
        test_env
            .client
            .jobs_registry
            .lock()
            .await
            .jobs
            .get_mut("test_job")
            .unwrap()
            .state = JobState::Active(Pid::from_u32(0));
        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job").await?.status
        );
        let _ = test_env
            .client
            .create(
                "test_job",
                JobConfig {
                    job_type: JobType::RunSh("different".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec![]),
                    wait_for: None,
                    run_as: None,
                    log_buffer_capacity_mb: None,
                    log_timestamp: None,
                },
            )
            .await
            .unwrap_err();
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_client_stop() -> Result<()> {
        let test_env = TestEnv::setup().await?;

        // stop missing
        let _ = test_env.client.stop("missing_job").await.unwrap_err();
        let test_job_dir = test_env.jobs_dir.join("test_job");
        fs::create_dir_all(&test_job_dir).await?;
        let active_job_dir = test_env.jobs_dir.join("test_active_job");
        fs::create_dir_all(&active_job_dir).await?;

        // stop inactive
        test_env.client.jobs_registry.lock().await.jobs.insert(
            "test_job".to_owned(),
            Job::new(
                test_job_dir.clone(),
                dummy_job_config(),
                JobState::Inactive(JobStatus::Pending {
                    waiting_for: vec![],
                }),
            ),
        );
        test_env.client.stop("test_job").await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.info("test_job").await?.status
        );
        let saved_status = jobs::load_status(&test_job_dir)?;
        assert_eq!(JobStatus::Stopped, saved_status);

        // stop active
        test_env.client.jobs_registry.lock().await.jobs.insert(
            "test_active_job".to_owned(),
            Job::new(
                active_job_dir.clone(),
                dummy_job_config(),
                JobState::Active(Pid::from_u32(0)),
            ),
        );
        test_env.client.stop("test_active_job").await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.info("test_active_job").await?.status
        );
        let saved_status = jobs::load_status(&active_job_dir)?;
        assert_eq!(JobStatus::Stopped, saved_status);
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_load_jobs() -> Result<()> {
        let test_env = TestEnv::setup().await?;
        let mut jobs_context = JobsContext {
            jobs: HashMap::new(),
            node_env: None,
            jobs_dir: test_env.jobs_dir.clone(),
            connector: utils::tests::DummyConnector {
                tmp_dir: test_env.tmp_dir.clone(),
            },
        };
        let pending_job_dir = test_env.jobs_dir.join("pending_job");
        fs::create_dir_all(&pending_job_dir).await?;
        let finished_job_dir = test_env.jobs_dir.join("finished_job");
        fs::create_dir_all(&finished_job_dir).await?;
        let active_job_dir = test_env.jobs_dir.join("active_job");
        fs::create_dir_all(&active_job_dir).await?;
        let invalid_job_dir = test_env.jobs_dir.join("invalid");
        fs::create_dir_all(&invalid_job_dir).await?;

        // no jobs
        load_jobs(
            &mut jobs_context,
            &test_env.test_job_runner_path.to_string_lossy(),
        )
        .await?;
        assert!(jobs_context.jobs.is_empty());

        // load active and inactive jobs
        let config = dummy_job_config();
        jobs::save_config(&config, &pending_job_dir)?;
        jobs::save_config(&config, &finished_job_dir)?;
        jobs::save_status(
            &JobStatus::Finished {
                exit_code: Some(1),
                message: "job message".to_string(),
            },
            &finished_job_dir,
        )?;
        jobs::save_config(&config, &active_job_dir)?;
        test_env.create_infinite_job_runner();
        let mut job = Command::new(&test_env.test_job_runner_path)
            .args(["active_job"])
            .spawn()?;
        test_env.wait_for_job_runner("active_job").await?;
        let invalid_config_path = invalid_job_dir.join(jobs::CONFIG_FILENAME);
        {
            // create invalid config file to make sure it won't crash load and is removed after
            let mut invalid_config = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&invalid_config_path)
                .await?;
            invalid_config.write_all("gibberish\n".as_bytes()).await?;
        }

        load_jobs(
            &mut jobs_context,
            &test_env.test_job_runner_path.to_string_lossy(),
        )
        .await?;

        assert_eq!(
            Job::new(
                pending_job_dir,
                config.clone(),
                JobState::Inactive(JobStatus::Pending {
                    waiting_for: vec![]
                })
            ),
            *jobs_context.jobs.get("pending_job").unwrap()
        );
        assert_eq!(
            Job::new(
                finished_job_dir,
                config.clone(),
                JobState::Inactive(JobStatus::Finished {
                    exit_code: Some(1),
                    message: "job message".to_string(),
                }),
            ),
            *jobs_context.jobs.get("finished_job").unwrap()
        );
        assert_eq!(
            Job::new(
                active_job_dir,
                config.clone(),
                JobState::Active(Pid::from_u32(job.id().unwrap())),
            ),
            *jobs_context.jobs.get("active_job").unwrap()
        );
        job.kill().await?;
        assert!(!invalid_config_path.exists());

        // invalid dir
        fs::remove_dir_all(&test_env.jobs_dir).await?;
        let _ = load_jobs(
            &mut jobs_context,
            &test_env.test_job_runner_path.to_string_lossy(),
        )
        .await
        .unwrap_err();
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_job_shutdown() -> Result<()> {
        let mut test_env = TestEnv::setup().await?;
        test_env
            .client
            .create("test_job", dummy_job_config())
            .await?;
        test_env.client.start("test_job").await?;
        test_env.create_infinite_job_runner();

        test_env
            .client
            .jobs_manager_state_tx
            .send(JobsManagerState::NotReady)?;
        let monitor_handle = test_env.spawn_monitor();

        assert_eq!(
            JobStatus::Pending {
                waiting_for: vec![]
            },
            test_env.client.info("test_job").await?.status
        );

        let _ = test_env.wait_for_job_runner("test_job").await.unwrap_err();
        test_env.client.startup(Default::default()).await?;
        test_env.wait_for_job_runner("test_job").await?;

        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job").await?.status
        );
        test_env.client.shutdown(false).await?;

        monitor_handle.await?;

        let mut sys = System::new();
        sys.refresh_processes();
        let ps = sys.processes();
        assert!(find_processes(
            &test_env.test_job_runner_path.to_string_lossy(),
            &["test_job"],
            ps
        )
        .next()
        .is_none());
        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job").await?.status
        );
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_missing_job_runner() -> Result<()> {
        let mut test_env = TestEnv::setup().await?;
        test_env
            .client
            .create("test_job", dummy_job_config())
            .await?;
        test_env.client.start("test_job").await?;

        let monitor_handle = test_env.spawn_monitor();

        while matches!(
            test_env.client.info("test_job").await?.status,
            JobStatus::Pending { .. }
        ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let info = test_env.client.info("test_job").await?;
        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: "failed to start job 'test_job': No such file or directory (os error 2)"
                    .to_string(),
            },
            info.status
        );
        assert_eq!(0, info.restart_count);
        assert!(info
            .logs
            .first()
            .unwrap()
            .contains("failed to start job 'test_job': No such file or directory (os error 2)"));

        test_env.run.stop();
        monitor_handle.await?;
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_dependencies() -> Result<()> {
        let mut test_env = TestEnv::setup().await?;
        test_env.create_infinite_job_runner();
        test_env
            .client
            .create(
                "test_invalid_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec!["invalid_dependency".to_string()]),
                    wait_for: None,
                    run_as: None,
                    log_buffer_capacity_mb: None,
                    log_timestamp: None,
                },
            )
            .await?;
        test_env.client.start("test_invalid_job").await?;
        test_env
            .client
            .create("test_job_a", dummy_job_config())
            .await?;
        test_env
            .client
            .create("test_job_b", dummy_job_config())
            .await?;
        test_env.client.start("test_job_a").await?;
        test_env.client.start("test_job_b").await?;
        test_env
            .client
            .create(
                "test_pending_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec!["test_job_a".to_string()]),
                    wait_for: Some(vec!["test_job_b".to_string()]),
                    run_as: None,
                    log_buffer_capacity_mb: None,
                    log_timestamp: None,
                },
            )
            .await?;
        test_env.client.start("test_pending_job").await?;

        let monitor_handle = test_env.spawn_monitor();
        test_env.wait_for_job_runner("test_job_a").await?;
        test_env.wait_for_job_runner("test_job_b").await?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: "job 'test_invalid_job' needs 'invalid_dependency', but it is not defined"
                    .to_string()
            },
            test_env.client.info("test_invalid_job").await?.status
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job_a").await?.status
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job_b").await?.status
        );
        assert_eq!(
            JobStatus::Pending {
                waiting_for: vec!["test_job_a".to_string(), "test_job_b".to_string()]
            },
            test_env.client.info("test_pending_job").await?.status
        );

        jobs::save_status(
            &JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string(),
            },
            &test_env.jobs_dir.join("test_job_a"),
        )?;
        jobs::save_status(&JobStatus::Stopped, &test_env.jobs_dir.join("test_job_b"))?;
        test_env.kill_job("test_job_a");
        test_env.wait_for_job_runner("test_job_a.finished").await?;
        test_env.kill_job("test_job_b");
        test_env.wait_for_job_runner("test_job_b.finished").await?;
        test_env.wait_for_job_runner("test_pending_job").await?;
        assert_eq!(
            JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string(),
            },
            test_env.client.info("test_job_a").await?.status
        );
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.info("test_job_b").await?.status
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_pending_job").await?.status
        );

        test_env.run.stop();
        monitor_handle.await?;

        test_env.kill_job("test_pending_job");
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_monitor_failed_dependency() -> Result<()> {
        let mut test_env = TestEnv::setup().await?;
        let failed_job_dir = test_env.jobs_dir.join("failed_job");
        fs::create_dir_all(&failed_job_dir).await?;

        test_env.create_infinite_job_runner();
        test_env
            .client
            .create("test_job", dummy_job_config())
            .await?;
        test_env.client.start("test_job").await?;
        test_env
            .client
            .create(
                "test_pending_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec!["failed_job".to_string()]),
                    wait_for: None,
                    run_as: None,
                    log_buffer_capacity_mb: None,
                    log_timestamp: None,
                },
            )
            .await?;
        test_env.client.start("test_pending_job").await?;
        // emulate failed job
        jobs::save_status(
            &JobStatus::Finished {
                exit_code: Some(1),
                message: "some job error".to_string(),
            },
            &failed_job_dir,
        )?;
        test_env.client.jobs_registry.lock().await.jobs.insert(
            "failed_job".to_string(),
            Job::new(
                failed_job_dir,
                dummy_job_config(),
                JobState::Active(Pid::from_u32(0)),
            ),
        );

        let monitor_handle = test_env.spawn_monitor();
        test_env.wait_for_job_runner("test_job").await?;

        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: "job 'test_pending_job' needs 'failed_job', but it failed with Some(1) - some job error"
                    .to_string()
            },
            test_env.client.info("test_pending_job").await?.status
        );
        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job").await?.status
        );

        test_env.run.stop();
        monitor_handle.await?;

        test_env.kill_job("test_job");
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn test_monitor_restart_crashed_job() -> Result<()> {
        let mut test_env = TestEnv::setup().await?;
        test_env.create_infinite_job_runner();
        let monitor_handle = test_env.spawn_monitor();

        test_env
            .client
            .create("test_job", dummy_job_config())
            .await?;
        test_env.client.start("test_job").await?;
        test_env.wait_for_job_runner("test_job").await.unwrap();

        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job").await?.status
        );

        test_env.kill_job("test_job");
        test_env
            .client
            .create(
                "test_restarting_job",
                JobConfig {
                    job_type: JobType::RunSh("".to_string()),
                    restart: RestartPolicy::Always(RestartConfig {
                        backoff_timeout_ms: 0,
                        backoff_base_ms: 0,
                        max_retries: None,
                    }),
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: None,
                    wait_for: None,
                    run_as: None,
                    log_buffer_capacity_mb: None,
                    log_timestamp: None,
                },
            )
            .await?;
        test_env.client.start("test_restarting_job").await?;
        test_env
            .wait_for_job_runner("test_restarting_job")
            .await
            .unwrap();

        let info = test_env.client.info("test_job").await?;
        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: format!("can't load job 'test_job' status from file after it stopped, with: Failed to read job status file `{}`: No such file or directory (os error 2)",
                                 test_env.jobs_dir.join("test_job").join(jobs::STATUS_FILENAME).display()),
            },
            info.status
        );
        assert_eq!(0, info.restart_count);
        assert!(
            info.logs.first().unwrap().contains("can't load job 'test_job' status from file after it stopped, with: Failed to read job status file")
        );
        assert_eq!(
            JobInfo {
                status: JobStatus::Running,
                progress: Default::default(),
                restart_count: 0,
                logs: vec![],
                upgrade_blocking: false,
            },
            test_env.client.info("test_restarting_job").await?
        );

        test_env.kill_job("test_restarting_job");
        test_env
            .wait_for_job_runner("test_restarting_job")
            .await
            .unwrap();

        let mut info = test_env.client.info("test_restarting_job").await?;
        assert_eq!(JobStatus::Running, info.status);
        assert_eq!(1, info.restart_count);
        assert!(
            info.logs.pop().unwrap().contains("can't load job 'test_restarting_job' status from file after it stopped, with: Failed to read job status file")
        );
        assert!(info
            .logs
            .pop()
            .unwrap()
            .contains("babel_job_runner process ended unexpectedly"));

        test_env.run.stop();
        monitor_handle.await?;

        test_env.kill_job("test_restarting_job");
        test_env.server.assert().await;
        Ok(())
    }
}
