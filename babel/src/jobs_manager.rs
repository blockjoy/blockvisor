/// Jobs Manager consists of three parts:
/// .1 Client - allow asynchronously request operations on jobs (like start, stop and get info)
///             and other interactions with `Manager`
/// .2 Monitor - service listening `job_runner`s for job related logs and restart info
/// .3 Manager - background worker that maintains job runners and take proper actions when some job runner ends
use crate::{
    async_pid_watch::AsyncPidWatch,
    babel_service::JobRunnerLock,
    jobs::{
        self, Job, JobState, JobsContext, JobsData, JobsRegistry, CONFIG_SUBDIR, STATUS_SUBDIR,
    },
    pal::BabelEngineConnector,
};
use async_trait::async_trait;
use babel_api::engine::{
    JobConfig, JobInfo, JobProgress, JobStatus, JobsInfo, PosixSignal, RestartPolicy,
    DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS,
};
use bv_utils::system::kill_all_processes;
use bv_utils::{run_flag::RunFlag, system::find_processes};
use bv_utils::{system::gracefully_terminate_process, with_retry};
use eyre::{bail, Context, ContextCompat, Report, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashMap, fs, fs::read_dir, path::Path, sync::Arc, time::Duration};
use sysinfo::{Pid, PidExt, Process, System, SystemExt};
use tokio::{
    select,
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
    connector: C,
    jobs_dir: &Path,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: &Path,
    state: JobsManagerState,
) -> Result<(Client<C>, Monitor, Manager<C>)> {
    let jobs_config_dir = jobs_dir.join(CONFIG_SUBDIR);
    if !jobs_config_dir.exists() {
        fs::create_dir_all(jobs_config_dir)?;
    }
    let jobs_status_dir = jobs_dir.join(STATUS_SUBDIR);
    if !jobs_status_dir.exists() {
        fs::create_dir_all(jobs_status_dir)?;
    }
    let mut jobs_context = JobsContext {
        jobs: HashMap::new(),
        jobs_data: JobsData::new(jobs_dir),
        connector,
    };
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
        jobs_context.jobs_data.jobs_config_dir.display()
    );
    let dir = read_dir(&jobs_context.jobs_data.jobs_config_dir).with_context(|| {
        format!(
            "failed to read jobs from dir {}",
            jobs_context.jobs_data.jobs_config_dir.display()
        )
    })?;
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
                        find_processes(job_runner_bin_path, &[&name], ps).next()
                    {
                        info!("{name} - Active(PID: {pid})");
                        JobState::Active(*pid)
                    } else {
                        let status = jobs_context
                            .jobs_data
                            .load_status(&name)
                            .unwrap_or(JobStatus::Pending);
                        info!("{name} - Inactive(status: {status:?})");
                        JobState::Inactive(status)
                    };
                    jobs_context.jobs.insert(name, Job::new(state, config));
                }
                Err(err) => {
                    // invalid job config file log error, remove invalid file and go to next one
                    let err_msg = format!(
                        "invalid job '{}' config file {}, load failed with: {:#}",
                        name,
                        path.display(),
                        err
                    );
                    error!(err_msg);
                    let mut client = jobs_context.connector.connect();
                    let _ = with_retry!(client.bv_error(err_msg.clone()));

                    let _ = fs::remove_file(path);
                }
            }
        }
    }
    Ok(())
}

#[async_trait]
pub trait JobsManagerClient {
    async fn startup(&self) -> Result<()>;
    async fn get_active_jobs_shutdown_timeout(&self) -> Duration;
    async fn shutdown(&self, force: bool) -> Result<()>;
    async fn list(&self) -> Result<JobsInfo>;
    async fn create(&self, name: &str, config: JobConfig) -> Result<()>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn get_job_shutdown_timeout(&self, name: &str) -> Duration;
    async fn stop(&self, name: &str) -> Result<()>;
    async fn cleanup(&self, name: &str) -> Result<()>;
    async fn info(&self, name: &str) -> Result<JobInfo>;
}

pub struct Client<C> {
    jobs_registry: JobsRegistry<C>,
    job_runner_bin_path: String,
    job_added_tx: watch::Sender<()>,
    jobs_manager_state_tx: watch::Sender<JobsManagerState>,
}

#[async_trait]
impl<C: BabelEngineConnector + Send> JobsManagerClient for Client<C> {
    async fn startup(&self) -> Result<()> {
        info!("Startup jobs manager - load jobs and set state to 'Ready'");
        let mut jobs_context = self.jobs_registry.lock().await;
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
                (
                    name.clone(),
                    build_job_info(job, jobs_context.jobs_data.load_progress(name)),
                )
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

        jobs_context.jobs_data.clear_status(name)?;
        jobs_context
            .jobs_data
            .save_config(&config, name)
            .with_context(|| {
                format!("failed to create job '{name}', can't save job config to file")
            })?;
        jobs_context.jobs.insert(
            name.to_string(),
            Job::new(JobState::Inactive(JobStatus::Stopped), config),
        );
        jobs_context
            .jobs_data
            .save_status(&JobStatus::Stopped, name)?;
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
                    *status = JobStatus::Pending;
                    jobs_context.jobs_data.clear_status(name)?;
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
            jobs_context
                .jobs_data
                .save_status(&JobStatus::Stopped, name)?;
        } else {
            bail!("can't stop, job '{name}' not found")
        }
        Ok(())
    }

    async fn cleanup(&self, name: &str) -> Result<()> {
        info!("Requested '{name} job to cleanup'");
        let config = {
            let jobs_context = self.jobs_registry.lock().await;
            if let Some(job) = jobs_context.jobs.get(name) {
                if let JobState::Inactive(_) = job.state {
                    job.config.clone()
                } else {
                    bail!("can't cleanup active job '{name}'");
                }
            } else {
                bail!("can't cleanup, job '{name}' not found");
            }
        };
        JobsData::cleanup_job(&config)?;
        Ok(())
    }

    async fn info(&self, name: &str) -> Result<JobInfo> {
        let jobs_context = &mut *self.jobs_registry.lock().await;
        let job = jobs_context
            .jobs
            .get_mut(name)
            .with_context(|| format!("unknown status, job '{name}' not found"))?;
        job.update();
        Ok(build_job_info(
            job,
            jobs_context.jobs_data.load_progress(name),
        ))
    }
}

fn build_job_info(job: &Job, progress: Option<JobProgress>) -> JobInfo {
    let restart_count = job.restart_stamps.len();
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
                    self.handle_stopped_job(
                        name,
                        job,
                        &jobs_context.jobs_data,
                        &jobs_context.connector,
                    )
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
                config: JobConfig { needs, .. },
                ..
            } = job
            {
                if *status == JobStatus::Pending {
                    if let Some(needs) = needs {
                        match deps_finished(name, &deps, needs) {
                            Ok(true) => {
                                info!("all '{name}' job dependencies finished");
                                self.start_job(
                                    name,
                                    job,
                                    &jobs_context.jobs_data,
                                    &jobs_context.connector,
                                )
                                .await;
                            }
                            Ok(false) => {}
                            Err(err) => {
                                *status = JobStatus::Finished {
                                    exit_code: None,
                                    message: err.to_string(),
                                };
                                let save_result = jobs_context.jobs_data.save_status(status, name);
                                let message = err.to_string();
                                job.push_log(&message);
                                warn!(message);
                                if let Err(err) = save_result {
                                    // if we can't save status for some reason, just log
                                    let message = format!(
                                        "failed to save failed job '{name}' status: {err:#}"
                                    );
                                    job.push_log(&message);
                                    error!(message);
                                    let mut client = jobs_context.connector.connect();
                                    let _ = with_retry!(client.bv_error(message.clone()));
                                }
                            }
                        }
                    } else {
                        self.start_job(name, job, &jobs_context.jobs_data, &jobs_context.connector)
                            .await
                    }
                }
            }
        }
    }

    async fn handle_stopped_job(
        &self,
        name: &str,
        job: &mut Job,
        jobs_data: &JobsData,
        connector: &C,
    ) {
        let status = match jobs_data.load_status(name) {
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
        if let JobStatus::Finished { .. } = status {
            info!("job '{name}' finished with {status:?}");
            job.state = JobState::Inactive(status)
        } else {
            job.register_restart();
            job.push_log("babel_job_runner process ended unexpectedly");
            warn!("job '{name}' process ended, but job was not finished - try restart");
            self.start_job(name, job, jobs_data, connector).await;
        };
    }

    async fn start_job(&self, name: &str, job: &mut Job, jobs_data: &JobsData, connector: &C) {
        if let Err(err) = jobs_data.clear_status(name) {
            let message = format!("failed to clear job '{name}' status, before new run: {err:#}");
            job.push_log(&message);
            error!(message);
            let mut client = connector.connect();
            let _ = with_retry!(client.bv_error(message.clone()));
            job.state = JobState::Inactive(JobStatus::Pending);
        } else {
            job.state = match self.start_job_runner(name).await {
                Ok(pid) => {
                    info!("started job '{name}' with PID {pid}");
                    JobState::Active(pid)
                }
                Err(err) => {
                    let message = format!("failed to start job '{name}': {err:#}");
                    job.push_log(&message);
                    warn!(message);
                    let status = JobStatus::Finished {
                        exit_code: None,
                        message,
                    };
                    match jobs_data.save_status(&status, name) {
                        Ok(()) => JobState::Inactive(status),
                        Err(_) => {
                            let message =
                                format!("failed to save failed job '{name}' status: {err:#}");
                            job.push_log(&message);
                            error!(message);
                            let mut client = connector.connect();
                            let _ = with_retry!(client.bv_error(message.clone()));
                            JobState::Inactive(JobStatus::Pending)
                        }
                    }
                }
            };
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
    use crate::utils;
    use assert_fs::TempDir;
    use babel_api::engine::{JobType, PosixSignal, RestartConfig};
    use bv_tests_utils::rpc::TestServer;
    use bv_utils::system::find_processes;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::process::Command;
    use tokio::sync::RwLock;
    use tokio::task::JoinHandle;

    struct TestEnv {
        tmp_dir: PathBuf,
        ctrl_file: PathBuf,
        jobs_dir: PathBuf,
        jobs_config_dir: PathBuf,
        jobs_status_dir: PathBuf,
        test_job_runner_path: PathBuf,
        run: RunFlag,
        client: Client<utils::tests::DummyConnector>,
        manager: Option<Manager<utils::tests::DummyConnector>>,
        server: TestServer,
    }

    impl TestEnv {
        async fn setup() -> Result<Self> {
            let tmp_dir = TempDir::new()?.to_path_buf();
            fs::create_dir_all(&tmp_dir)?;
            let ctrl_file = tmp_dir.join("job_runner_started");
            let jobs_dir = tmp_dir.join("jobs");
            let jobs_config_dir = jobs_dir.join(CONFIG_SUBDIR);
            let jobs_status_dir = jobs_dir.join(STATUS_SUBDIR);
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
                utils::tests::DummyConnector {
                    tmp_dir: tmp_dir.clone(),
                },
                &jobs_dir,
                Arc::new(RwLock::new(Some(0))),
                &test_job_runner_path,
                JobsManagerState::Ready,
            )
            .await?;
            Ok(Self {
                tmp_dir,
                ctrl_file,
                jobs_dir,
                jobs_config_dir,
                jobs_status_dir,
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
            utils::tests::create_dummy_bin(&self.test_job_runner_path, &self.ctrl_file, true);
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
            run_as: None,
            log_buffer_capacity_mb: None,
        }
    }

    #[tokio::test]
    async fn test_client_create_max_jobs() -> Result<()> {
        let test_env = TestEnv::setup().await?;
        let _ = test_env.client.info("missing_job").await.unwrap_err();

        // start OK
        let status_path = test_env.jobs_status_dir.join("test_job.status");
        {
            // create dummy status file to make sure it is removed after start
            let mut status_file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&status_path)?;
            writeln!(status_file, "empty")?;
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

        // start OK
        let status_path = test_env.jobs_status_dir.join("test_job.status");
        {
            // create dummy status file to make sure it is removed after start
            let mut status_file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&status_path)?;
            writeln!(status_file, "empty")?;
        }
        let config = dummy_job_config();
        test_env.client.create("test_job", config.clone()).await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.info("test_job").await?.status
        );
        test_env.client.start("test_job").await?;
        assert!(!status_path.exists());
        let saved_config = jobs::load_config(&test_env.jobs_config_dir.join("test_job.cfg"))?;
        assert_eq!(config, saved_config);
        assert!(test_env
            .manager
            .unwrap()
            .job_added_rx
            .has_changed()
            .unwrap());
        assert_eq!(
            JobInfo {
                status: JobStatus::Pending,
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
                    run_as: None,
                    log_buffer_capacity_mb: None,
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

        // stop inactive
        test_env.client.jobs_registry.lock().await.jobs.insert(
            "test_job".to_owned(),
            Job::new(JobState::Inactive(JobStatus::Pending), dummy_job_config()),
        );
        test_env.client.stop("test_job").await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.info("test_job").await?.status
        );
        let saved_status = jobs::load_status(&test_env.jobs_status_dir.join("test_job.status"))?;
        assert_eq!(JobStatus::Stopped, saved_status);

        // stop active
        test_env.client.jobs_registry.lock().await.jobs.insert(
            "test_active_job".to_owned(),
            Job::new(JobState::Active(Pid::from_u32(0)), dummy_job_config()),
        );
        test_env.client.stop("test_active_job").await?;
        assert_eq!(
            JobStatus::Stopped,
            test_env.client.info("test_active_job").await?.status
        );
        let saved_status =
            jobs::load_status(&test_env.jobs_status_dir.join("test_active_job.status"))?;
        assert_eq!(JobStatus::Stopped, saved_status);
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_load_jobs() -> Result<()> {
        let test_env = TestEnv::setup().await?;
        let mut jobs_context = JobsContext {
            jobs: HashMap::new(),
            jobs_data: JobsData::new(&test_env.jobs_dir),
            connector: utils::tests::DummyConnector {
                tmp_dir: test_env.tmp_dir.clone(),
            },
        };
        // no jobs
        load_jobs(
            &mut jobs_context,
            &test_env.test_job_runner_path.to_string_lossy(),
        )
        .await?;
        assert!(jobs_context.jobs.is_empty());

        // load active and inactive jobs
        let config = dummy_job_config();
        jobs_context.jobs_data.save_config(&config, "pending_job")?;
        jobs_context
            .jobs_data
            .save_config(&config, "finished_job")?;
        jobs_context.jobs_data.save_status(
            &JobStatus::Finished {
                exit_code: Some(1),
                message: "job message".to_string(),
            },
            "finished_job",
        )?;
        jobs_context.jobs_data.save_config(&config, "active_job")?;
        test_env.create_infinite_job_runner();
        let mut job = Command::new(&test_env.test_job_runner_path)
            .args(["active_job"])
            .spawn()?;
        test_env.wait_for_job_runner().await?;
        let invalid_config_path = test_env.jobs_config_dir.join("invalid.cfg");
        {
            // create invalid config file to make sure it won't crash load and is removed after
            let mut invalid_config = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&invalid_config_path)?;
            writeln!(invalid_config, "gibberish")?;
        }

        load_jobs(
            &mut jobs_context,
            &test_env.test_job_runner_path.to_string_lossy(),
        )
        .await?;

        assert_eq!(
            Job::new(JobState::Inactive(JobStatus::Pending), config.clone()),
            *jobs_context.jobs.get("pending_job").unwrap()
        );
        assert_eq!(
            Job::new(
                JobState::Inactive(JobStatus::Finished {
                    exit_code: Some(1),
                    message: "job message".to_string(),
                }),
                config.clone(),
            ),
            *jobs_context.jobs.get("finished_job").unwrap()
        );
        assert_eq!(
            Job::new(
                JobState::Active(Pid::from_u32(job.id().unwrap())),
                config.clone()
            ),
            *jobs_context.jobs.get("active_job").unwrap()
        );
        job.kill().await?;
        assert!(!invalid_config_path.exists());

        // invalid dir
        fs::remove_dir_all(&test_env.jobs_dir)?;
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
            JobStatus::Pending,
            test_env.client.info("test_job").await?.status
        );

        let _ = test_env.wait_for_job_runner().await.unwrap_err();
        test_env.client.startup().await?;
        test_env.wait_for_job_runner().await?;

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

        while JobStatus::Pending == test_env.client.info("test_job").await?.status {
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
                    run_as: None,
                    log_buffer_capacity_mb: None,
                },
            )
            .await?;
        test_env.client.start("test_invalid_job").await?;
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
                    needs: Some(vec!["test_job".to_string()]),
                    run_as: None,
                    log_buffer_capacity_mb: None,
                },
            )
            .await?;
        test_env.client.start("test_pending_job").await?;

        let monitor_handle = test_env.spawn_monitor();
        test_env.wait_for_job_runner().await?;

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
            test_env.client.info("test_job").await?.status
        );
        assert_eq!(
            JobStatus::Pending,
            test_env.client.info("test_pending_job").await?.status
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
            test_env.client.info("test_job").await?.status
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
                    run_as: None,
                    log_buffer_capacity_mb: None,
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
            "failed_job",
            &test_env.jobs_status_dir,
        )?;
        test_env.client.jobs_registry.lock().await.jobs.insert(
            "failed_job".to_owned(),
            Job::new(JobState::Active(Pid::from_u32(0)), dummy_job_config()),
        );

        let monitor_handle = test_env.spawn_monitor();
        test_env.wait_for_job_runner().await?;

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
        test_env.wait_for_job_runner().await.unwrap();

        assert_eq!(
            JobStatus::Running,
            test_env.client.info("test_job").await?.status
        );

        fs::remove_file(&test_env.ctrl_file)?;
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
                    run_as: None,
                    log_buffer_capacity_mb: None,
                },
            )
            .await?;
        test_env.client.start("test_restarting_job").await?;
        test_env.wait_for_job_runner().await.unwrap();

        let info = test_env.client.info("test_job").await?;
        assert_eq!(
            JobStatus::Finished {
                exit_code: None,
                message: format!("can't load job 'test_job' status from file after it stopped, with: Failed to read job status file `{}`: No such file or directory (os error 2)",
                                 test_env.jobs_status_dir.join("test_job.status").display()),
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

        fs::remove_file(&test_env.ctrl_file)?;
        test_env.kill_job("test_restarting_job");
        test_env.wait_for_job_runner().await.unwrap();

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
