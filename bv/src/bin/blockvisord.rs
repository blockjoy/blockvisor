use blockvisord::linux_platform::bv_root;
use blockvisord::{blockvisord::BlockvisorD, config};
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
    let config = config::Config::load(&bv_root()).await?;
    match config.pal.as_ref().unwrap_or(&config::PalConfig::LinuxFc) {
        config::PalConfig::LinuxFc => {
            let pal = blockvisord::linux_fc_platform::LinuxFcPlatform::new().await?;
            BlockvisorD::new(pal, config).await?.run(run).await?;
        }
        config::PalConfig::LinuxBare => {
            let pal = blockvisord::linux_bare_platform::LinuxBarePlatform::new().await?;
            BlockvisorD::new(pal, config).await?.run(run).await?;
        }
        config::PalConfig::LinuxApptainer(apptainer_config) => {
            let pal = blockvisord::linux_apptainer_platform::LinuxApptainerPlatform::new(
                &config.iface,
                apptainer_config.clone(),
            )
            .await?;
            BlockvisorD::new(pal, config).await?.run(run).await?;
        }
    }
    Ok(())
}
