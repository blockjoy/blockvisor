use anyhow::Result;
use blockvisord::blockvisord::BlockvisorD;
use blockvisord::linux_platform::LinuxPlatform;
use blockvisord::logging::setup_logging;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let pal = LinuxPlatform::new()?;
    BlockvisorD { pal }.run().await?;

    info!("Stopping...");
    Ok(())
}
