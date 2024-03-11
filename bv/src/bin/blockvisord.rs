use blockvisord::blockvisord::BlockvisorD;
use blockvisord::linux_platform::LinuxPlatform;
use bv_utils::logging::setup_logging;
use bv_utils::run_flag::RunFlag;
use eyre::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    let run = RunFlag::run_until_ctrlc();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let (pal, config) = LinuxPlatform::new_with_config().await?;
    BlockvisorD::new(pal, config).await?.run(run).await?;
    Ok(())
}
