use blockvisord::linux_platform::bv_root;
use blockvisord::{blockvisord::BlockvisorD, config};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging();
    let run = RunFlag::run_until_ctrlc();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let config = config::Config::load(&bv_root()).await?;
    let pal = blockvisord::linux_apptainer_platform::LinuxApptainerPlatform::new(
        &config.iface,
        config.apptainer.clone(),
    )
    .await?;
    BlockvisorD::new(pal, config).await?.run(run).await?;
    Ok(())
}
