use babel::{
    babel::BABEL_CONFIG_PATH,
    chroot_platform::UdsConnector,
    download_job::Downloader,
    is_protocol_data_locked,
    job_runner::{save_job_status, ArchiveJobRunner, TransferConfig},
    jobs::{self, ARCHIVE_JOBS_META_DIR},
    load_config,
    log_buffer::LogBuffer,
    pal::BabelEngineConnector,
    run_sh_job::RunShJob,
    upload_job::Uploader,
    PROTOCOL_DATA_LOCK_FILENAME,
};
use babel_api::engine::{
    Compression, JobStatus, JobType, DEFAULT_JOB_SHUTDOWN_SIGNAL, DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS,
};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::{anyhow, bail};
use std::{
    io::Write,
    path::PathBuf,
    {env, fs, time::Duration},
};
use tokio::join;
use tracing::{error, info, warn};

/// Logs are forwarded asap to log server, so we don't need big buffer, only to buffer logs during some
/// temporary log server unavailability (e.g. while updating).
const DEFAULT_LOG_BUFFER_CAPACITY_MB: usize = 128;
const DEFAULT_MAX_DOWNLOAD_CONNECTIONS: usize = 3;
const DEFAULT_MAX_UPLOAD_CONNECTIONS: usize = 3;
const DEFAULT_MAX_RUNNERS: usize = 8;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> eyre::Result<()> {
    setup_logging();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    // use `setsid()` to make sure job runner won't be killed when babel is stopped with SIGINT
    let _ = nix::unistd::setsid();
    match get_job_name() {
        Ok(job_name) => {
            let res = run_job(job_name, UdsConnector).await;
            if let Err(err) = &res {
                warn!("JobRunner failed with: {err:#}");
            }
            res
        }
        Err(err) => {
            error!("{err:#}");
            Err(err)
        }
    }
}

fn get_job_name() -> eyre::Result<String> {
    let mut args = env::args();
    let job_name = args
        .nth(1)
        .ok_or_else(|| anyhow!("Missing argument! Expected unique job name."))?;
    if args.count() != 0 {
        bail!("Invalid number of arguments! Expected only one argument: unique job name.");
    }
    Ok(job_name)
}

async fn run_job(
    job_name: String,
    connector: impl BabelEngineConnector + Copy + Send + Sync + 'static,
) -> eyre::Result<()> {
    let mut run = RunFlag::run_until_ctrlc();
    let job_dir = jobs::JOBS_DIR.join(&job_name);
    let job_config = jobs::load_config(&job_dir)?;
    let babel_config = load_config(&BABEL_CONFIG_PATH).await?;

    if job_config.protocol_data_lock.unwrap_or_default() {
        let data_lock = babel_config
            .node_env
            .data_mount_point
            .join(PROTOCOL_DATA_LOCK_FILENAME);
        if !data_lock.exists() {
            fs::File::create(data_lock)?;
        }
    }
    match job_config.job_type {
        JobType::RunSh(body) => {
            let log_buffer = LogBuffer::default();
            let log_handler = tokio::spawn(run_log_handler(
                run.clone(),
                log_buffer.subscribe(),
                job_dir.join(jobs::LOGS_FILENAME),
                job_config
                    .log_buffer_capacity_mb
                    .unwrap_or(DEFAULT_LOG_BUFFER_CAPACITY_MB),
            ));
            let _ = join!(
                RunShJob {
                    timer: bv_utils::timer::SysTimer,
                    sh_body: body,
                    restart_policy: job_config.restart,
                    shutdown_timeout: Duration::from_secs(
                        job_config
                            .shutdown_timeout_secs
                            .unwrap_or(DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS)
                    ),
                    shutdown_signal: job_config
                        .shutdown_signal
                        .unwrap_or(DEFAULT_JOB_SHUTDOWN_SIGNAL),
                    log_buffer,
                    log_timestamp: job_config.log_timestamp.unwrap_or(false),
                    run_as: job_config.run_as,
                }
                .run(run, &job_name, &jobs::JOBS_DIR),
                log_handler
            );
        }
        JobType::Download {
            max_connections,
            max_runners,
        } => {
            if is_protocol_data_locked(&babel_config.node_env.data_mount_point) {
                save_job_status(
                    &JobStatus::Finished {
                        exit_code: Some(0),
                        message: format!("job '{job_name}' finished"),
                    },
                    &job_name,
                    &jobs::JOBS_DIR,
                )
                .await;
            } else {
                ArchiveJobRunner::new(
                    bv_utils::timer::SysTimer,
                    job_config.restart,
                    Downloader::new(
                        connector,
                        babel_config.node_env.protocol_data_path,
                        build_transfer_config(
                            babel_config
                                .node_env
                                .data_mount_point
                                .join(ARCHIVE_JOBS_META_DIR),
                            job_dir.join(jobs::PROGRESS_FILENAME),
                            None,
                            max_connections.unwrap_or(DEFAULT_MAX_DOWNLOAD_CONNECTIONS),
                            max_runners.unwrap_or(DEFAULT_MAX_RUNNERS),
                        )?,
                    ),
                )
                .run(run, &job_name, &jobs::JOBS_DIR)
                .await;
            }
        }
        JobType::Upload {
            exclude,
            compression,
            max_connections,
            max_runners,
            number_of_chunks,
            url_expires_secs,
            data_version,
        } => {
            ArchiveJobRunner::new(
                bv_utils::timer::SysTimer,
                job_config.restart,
                Uploader::new(
                    connector,
                    babel_config.node_env.protocol_data_path,
                    exclude.unwrap_or_default(),
                    number_of_chunks,
                    url_expires_secs,
                    data_version,
                    build_transfer_config(
                        babel_config
                            .node_env
                            .data_mount_point
                            .join(ARCHIVE_JOBS_META_DIR),
                        job_dir.join(jobs::PROGRESS_FILENAME),
                        compression,
                        max_connections.unwrap_or(DEFAULT_MAX_UPLOAD_CONNECTIONS),
                        max_runners.unwrap_or(DEFAULT_MAX_RUNNERS),
                    )?,
                )?,
            )
            .run(run, &job_name, &jobs::JOBS_DIR)
            .await;
        }
    }
    Ok(())
}

fn build_transfer_config(
    archive_jobs_meta_dir: PathBuf,
    progress_file_path: PathBuf,
    compression: Option<Compression>,
    max_connections: usize,
    max_runners: usize,
) -> eyre::Result<TransferConfig> {
    if !archive_jobs_meta_dir.exists() {
        fs::create_dir_all(&archive_jobs_meta_dir)?;
    }
    TransferConfig::new(
        archive_jobs_meta_dir,
        progress_file_path,
        compression,
        max_connections,
        max_runners,
    )
}

async fn run_log_handler(
    mut log_run: RunFlag,
    mut log_rx: tokio::sync::broadcast::Receiver<String>,
    path: PathBuf,
    capacity: usize,
) {
    let mut log_file = file_rotate::FileRotate::new(
        path,
        file_rotate::suffix::AppendCount::new(2),
        file_rotate::ContentLimit::Bytes(capacity * 1_000_000),
        file_rotate::compression::Compression::OnRotate(0),
        None,
    );
    while log_run.load() {
        if let Some(Ok(log)) = log_run.select(log_rx.recv()).await {
            let _ = log_file.write_all(log.as_bytes());
        }
    }
}
