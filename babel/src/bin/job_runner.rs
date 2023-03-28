use babel::job_data::JOBS_DIR;
use babel::job_runner::JobRunner;
use babel::log_buffer::LogBuffer;
use babel::logging;
use bv_utils::run_flag::RunFlag;
use eyre::{anyhow, bail};
use std::env;
use tokio::{join, select};
use tracing::info;

/// Logs are forwarded asap to log server, so we don't need big buffer, only to buffer logs during some
/// temporary log server unavailability (e.g. while updating).
const LOG_BUFFER_CAPACITY_LN: usize = 1024;

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
        while log_run.load() {
            select!(
                _log = log_rx.recv() => {
                        // TODO send log to Babel or other logs server
                    }
                _ = log_run.wait() => {}
            );
        }
    };
    join!(
        JobRunner::new(bv_utils::timer::SysTimer, &JOBS_DIR, job_name, log_buffer)?.run(run),
        log_handler
    );

    Ok(())
}
