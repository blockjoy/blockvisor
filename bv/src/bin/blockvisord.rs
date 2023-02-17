use anyhow::Result;
use blockvisord::blockvisord::BlockvisorD;
use blockvisord::linux_platform::LinuxPlatform;
use blockvisord::logging::setup_logging;

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    let pal = LinuxPlatform::new()?;
    BlockvisorD::new(pal).await?.run().await?;
    Ok(())
}
