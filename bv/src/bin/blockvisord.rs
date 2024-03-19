use blockvisord::{blockvisord::BlockvisorD, config, pal::Pal, pal_config};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
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
    match pal_config::PalConfig::load().await? {
        pal_config::PalConfig::LinuxFc => {
            let pal = blockvisord::linux_fc_platform::LinuxFcPlatform::new().await?;
            let config = config::Config::load(pal.bv_root()).await?;
            BlockvisorD::new(pal, config).await?.run(run).await?;
        }
        pal_config::PalConfig::LinuxBare => {
            let pal = blockvisord::linux_bare_platform::LinuxBarePlatform::new().await?;
            let config = config::Config::load(pal.bv_root()).await?;
            BlockvisorD::new(pal, config).await?.run(run).await?;
        }
    }
    Ok(())
}
