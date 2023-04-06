use babel::{job_runner::JobRunner, jobs, log_buffer::LogBuffer, logging, BABEL_LOGS_UDS_PATH};
use babel_api::logs_collector_client::LogsCollectorClient;
use bv_utils::run_flag::RunFlag;
use eyre::{anyhow, bail};
use std::{env, time::Duration};
use tokio::{
    net::UnixStream,
    {join, select},
};
use tonic::transport::{Endpoint, Uri};
use tracing::{debug, info};

/// Logs are forwarded asap to log server, so we don't need big buffer, only to buffer logs during some
/// temporary log server unavailability (e.g. while updating).
const LOG_BUFFER_CAPACITY_LN: usize = 1024;
const LOG_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let mut args = env::args();
    let job_name = args
        .nth(1)
        .ok_or_else(|| anyhow!("Missing argument! Expected unique job name."))?;
    if args.count() != 0 {
        bail!("Invalid number of arguments! Expected only one argument: unique job name.");
    }
    logging::setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let mut run = RunFlag::run_until_ctrlc();

    let log_buffer = LogBuffer::new(LOG_BUFFER_CAPACITY_LN);
    let mut log_rx = log_buffer.subscribe();
    let mut log_run = run.clone();
    let log_handler = async move {
        let mut client = LogsCollectorClient::new(
            Endpoint::from_static("http://[::]:50052")
                .timeout(Duration::from_secs(3))
                .connect_timeout(Duration::from_secs(3))
                .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                    UnixStream::connect(BABEL_LOGS_UDS_PATH.to_path_buf())
                })),
        );

        while log_run.load() {
            select!(
                log = log_rx.recv() => {
                    if let Ok(log) = log {
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
                _ = log_run.wait() => {}
            );
        }
    };
    join!(
        JobRunner::new(
            bv_utils::timer::SysTimer,
            &jobs::JOBS_DIR,
            job_name,
            log_buffer
        )?
        .run(run),
        log_handler
    );

    Ok(())
}
