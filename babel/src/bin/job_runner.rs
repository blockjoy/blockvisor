use babel::chroot_platform::UdsConnector;
use babel::pal::BabelEngineConnector;
use babel::{
    download_job::DownloadJob, job_runner::TransferConfig, jobs, log_buffer::LogBuffer,
    run_sh_job::RunShJob, upload_job::UploadJob, BABEL_LOGS_UDS_PATH,
};
use babel_api::engine::{
    Compression, DEFAULT_JOB_SHUTDOWN_SIGNAL, DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS,
};
use babel_api::{babel::logs_collector_client::LogsCollectorClient, engine::JobType};
use bv_utils::{logging::setup_logging, rpc::RPC_REQUEST_TIMEOUT, run_flag::RunFlag};
use eyre::{anyhow, bail};
use std::{env, fs, time::Duration};
use tokio::join;
use tracing::info;

/// Logs are forwarded asap to log server, so we don't need big buffer, only to buffer logs during some
/// temporary log server unavailability (e.g. while updating).
const LOG_BUFFER_CAPACITY_LN: usize = 1024;
const LOG_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_MAX_DOWNLOAD_CONNECTIONS: usize = 3;
const DEFAULT_MAX_UPLOAD_CONNECTIONS: usize = 3;
const DEFAULT_MAX_RUNNERS: usize = 8;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    setup_logging();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    // use `setsid()` to make sure job runner won't be killed when babel is stopped with SIGINT
    let _ = nix::unistd::setsid();
    let mut args = env::args();
    let job_name = args
        .nth(1)
        .ok_or_else(|| anyhow!("Missing argument! Expected unique job name."))?;
    if args.count() != 0 {
        bail!("Invalid number of arguments! Expected only one argument: unique job name.");
    }
    run_job(job_name, UdsConnector).await
}

async fn run_job(
    job_name: String,
    connector: impl BabelEngineConnector + Copy + Send + Sync + 'static,
) -> eyre::Result<()> {
    let mut run = RunFlag::run_until_ctrlc();

    let job_config = jobs::load_config(&jobs::config_file_path(
        &job_name,
        &jobs::JOBS_DIR.join(jobs::CONFIG_SUBDIR),
    ))?;
    match job_config.job_type {
        JobType::RunSh(body) => {
            let log_buffer = LogBuffer::new(LOG_BUFFER_CAPACITY_LN);
            let log_handler = run_log_handler(run.clone(), log_buffer.subscribe());
            join!(
                RunShJob::new(
                    bv_utils::timer::SysTimer,
                    body,
                    job_config.restart,
                    Duration::from_secs(
                        job_config
                            .shutdown_timeout_secs
                            .unwrap_or(DEFAULT_JOB_SHUTDOWN_TIMEOUT_SECS)
                    ),
                    job_config
                        .shutdown_signal
                        .unwrap_or(DEFAULT_JOB_SHUTDOWN_SIGNAL),
                    log_buffer,
                    job_config.run_as,
                )?
                .run(run, &job_name, &jobs::JOBS_DIR),
                log_handler
            );
        }
        JobType::Download {
            manifest,
            destination,
            max_connections,
            max_runners,
        } => {
            let Some(manifest) = manifest else {
                bail!("missing DownloadManifest")
            };
            let compression = manifest.compression;
            DownloadJob::new(
                bv_utils::timer::SysTimer,
                connector,
                manifest,
                destination.unwrap_or(babel_api::engine::BLOCKCHAIN_DATA_PATH.to_path_buf()),
                job_config.restart,
                build_transfer_config(
                    &job_name,
                    compression,
                    max_connections.unwrap_or(DEFAULT_MAX_DOWNLOAD_CONNECTIONS),
                    max_runners.unwrap_or(DEFAULT_MAX_RUNNERS),
                )?,
            )?
            .run(run, &job_name, &jobs::JOBS_DIR)
            .await;
        }
        JobType::Upload {
            manifest,
            source,
            exclude,
            compression,
            max_connections,
            max_runners,
            ..
        } => {
            UploadJob::new(
                bv_utils::timer::SysTimer,
                connector,
                manifest.ok_or(anyhow!("missing UploadManifest"))?,
                source.unwrap_or(babel_api::engine::BLOCKCHAIN_DATA_PATH.to_path_buf()),
                exclude.unwrap_or_default(),
                job_config.restart,
                build_transfer_config(
                    &job_name,
                    compression,
                    max_connections.unwrap_or(DEFAULT_MAX_UPLOAD_CONNECTIONS),
                    max_runners.unwrap_or(DEFAULT_MAX_RUNNERS),
                )?,
            )?
            .run(run, &job_name, &jobs::JOBS_DIR)
            .await;
        }
    }
    Ok(())
}

fn build_transfer_config(
    job_name: &str,
    compression: Option<Compression>,
    max_connections: usize,
    max_runners: usize,
) -> eyre::Result<TransferConfig> {
    if !jobs::ARCHIVE_JOBS_META_DIR.exists() {
        fs::create_dir_all(*jobs::ARCHIVE_JOBS_META_DIR)?;
    }
    TransferConfig::new(
        jobs::ARCHIVE_JOBS_META_DIR.to_path_buf(),
        jobs::progress_file_path(job_name, &jobs::JOBS_DIR.join(jobs::STATUS_SUBDIR)),
        compression,
        max_connections,
        max_runners,
    )
}

async fn run_log_handler(
    mut log_run: RunFlag,
    mut log_rx: tokio::sync::broadcast::Receiver<String>,
) {
    let mut client = LogsCollectorClient::with_interceptor(
        bv_utils::rpc::build_socket_channel(BABEL_LOGS_UDS_PATH),
        bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
    );

    while log_run.load() {
        if let Some(Ok(log)) = log_run.select(log_rx.recv()).await {
            while client.send_log(log.clone()).await.is_err() {
                // try to send log every 1s - log server may be temporarily unavailable
                log_run.select(tokio::time::sleep(LOG_RETRY_INTERVAL)).await;
                if !log_run.load() {
                    break;
                }
            }
        }
    }
}
