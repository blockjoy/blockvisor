use babel::job_data::JOBS_DIR;
use babel::job_runner::JobRunner;
use babel::logging;
use bv_utils::run_flag::RunFlag;
use eyre::{anyhow, bail};
use std::env;
use tracing::info;

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

    let run = RunFlag::run_until_ctrlc();
    JobRunner::new(bv_utils::timer::SysTimer, &JOBS_DIR, job_name)?
        .run(run)
        .await;

    Ok(())
}
