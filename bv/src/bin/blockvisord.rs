use blockvisord::blockvisord::BlockvisorD;
use blockvisord::config;
use blockvisord::linux_platform::LinuxPlatform;
use blockvisord::pal::Pal;
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
    let pal = LinuxPlatform::new().await?;
    let config = config::Config::load(pal.bv_root()).await?;
    BlockvisorD::new(pal, config).await?.run(run).await?;
    Ok(())
}
