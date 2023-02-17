use anyhow::Result;
use blockvisord::blockvisord::BlockvisorD;
use blockvisord::linux_platform::LinuxPlatform;

#[tokio::main]
async fn main() -> Result<()> {
    let pal = LinuxPlatform::new()?;
    BlockvisorD::new(pal).await?.run().await?;
    Ok(())
}
