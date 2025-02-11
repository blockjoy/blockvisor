use babel::{babel::BABEL_CONFIG_PATH, chroot_platform::UdsConnector, jobs};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::{anyhow, bail};
use std::env;
use tracing::{error, info, warn};

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
            let res = babel::job_runner::run_job(
                RunFlag::run_until_ctrlc(),
                job_name,
                &jobs::JOBS_DIR,
                babel::load_config(&BABEL_CONFIG_PATH).await?,
                UdsConnector,
            )
            .await;
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
