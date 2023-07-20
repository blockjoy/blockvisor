use babel::{
    download_job::DownloadJob, job_runner::TransferConfig, jobs, log_buffer::LogBuffer,
    run_sh_job::RunShJob, upload_job::UploadJob, BABEL_LOGS_UDS_PATH,
};
use babel_api::{babel::logs_collector_client::LogsCollectorClient, engine::JobType};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::{anyhow, bail};
use std::{env, time::Duration};
use tokio::{join, net::UnixStream};
use tonic::transport::{Endpoint, Uri};
use tracing::{debug, info};

/// Logs are forwarded asap to log server, so we don't need big buffer, only to buffer logs during some
/// temporary log server unavailability (e.g. while updating).
const LOG_BUFFER_CAPACITY_LN: usize = 1024;
const LOG_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // use `setsid()` to make sure job runner won't be killed when babel is stopped with SIGINT
    let _ = nix::unistd::setsid();
    let mut args = env::args();
    let job_name = args
        .nth(1)
        .ok_or_else(|| anyhow!("Missing argument! Expected unique job name."))?;
    if args.count() != 0 {
        bail!("Invalid number of arguments! Expected only one argument: unique job name.");
    }
    setup_logging();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );

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
                    log_buffer,
                )?
                .run(run, &job_name, &jobs::JOBS_DIR),
                log_handler
            );
        }
        JobType::Download {
            manifest,
            destination,
        } => {
            DownloadJob::new(
                bv_utils::timer::SysTimer,
                manifest.ok_or(anyhow!("missing DownloadManifest"))?,
                destination,
                job_config.restart,
                TransferConfig::new(
                    jobs::JOBS_DIR
                        .join(jobs::STATUS_SUBDIR)
                        .join(&format!("{job_name}.progress")),
                )?,
            )?
            .run(run, &job_name, &jobs::JOBS_DIR)
            .await;
        }
        JobType::Upload { manifest, source } => {
            UploadJob::new(
                bv_utils::timer::SysTimer,
                manifest.ok_or(anyhow!("missing UploadManifest"))?,
                source,
                job_config.restart,
                TransferConfig::new(
                    jobs::JOBS_DIR
                        .join(jobs::STATUS_SUBDIR)
                        .join(&format!("{job_name}.progress")),
                )?,
            )?
            .run(run, &job_name, &jobs::JOBS_DIR)
            .await;
        }
    }
    Ok(())
}

async fn run_log_handler(
    mut log_run: RunFlag,
    mut log_rx: tokio::sync::broadcast::Receiver<String>,
) {
    let mut client = LogsCollectorClient::new(
        Endpoint::from_static("http://[::]:50052")
            .timeout(Duration::from_secs(3))
            .connect_timeout(Duration::from_secs(3))
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                UnixStream::connect(BABEL_LOGS_UDS_PATH.to_path_buf())
            })),
    );

    while log_run.load() {
        if let Some(Ok(log)) = log_run.select(log_rx.recv()).await {
            while let Err(err) = client.send_log(log.clone()).await {
                debug!("send_log failed with: {err}");
                // try to send log every 1s - log server may be temporarily unavailable
                log_run.select(tokio::time::sleep(LOG_RETRY_INTERVAL)).await;
                if !log_run.load() {
                    break;
                }
            }
        }
    }
}
