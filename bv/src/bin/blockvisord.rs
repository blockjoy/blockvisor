use blockvisord::linux_platform::bv_root;
use blockvisord::{blockvisord::BlockvisorD, bv_config};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::Result;
use tracing::info;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    setup_logging();
    let run = RunFlag::run_until_ctrlc();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let config = bv_config::Config::load(&bv_root()).await?;
    let pal = blockvisord::apptainer_platform::ApptainerPlatform::new(&config).await?;
    BlockvisorD::new(pal, config).await?.run(run).await?;
    Ok(())
}
