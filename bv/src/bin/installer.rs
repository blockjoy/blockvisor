use async_trait::async_trait;
use blockvisord::installer::{self, Installer, INSTALL_PATH};
use blockvisord::linux_platform::bv_root;
use bv_utils::{cmd::run_cmd, logging::setup_logging, timer::SysTimer};
use eyre::{Context, Result};
use tracing::error;

struct SystemCtl;

#[async_trait]
impl installer::BvService for SystemCtl {
    async fn reload(&self) -> Result<()> {
        run_cmd("systemctl", ["daemon-reload"]).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        run_cmd("systemctl", ["stop", "blockvisor.service"]).await?;
        Ok(())
    }

    async fn start(&self) -> Result<()> {
        run_cmd("systemctl", ["start", "blockvisor.service"]).await?;
        Ok(())
    }

    async fn enable(&self) -> Result<()> {
        run_cmd("systemctl", ["enable", "blockvisor.service"]).await?;
        Ok(())
    }

    async fn ensure_active(&self) -> Result<()> {
        run_cmd("systemctl", ["is-active", "blockvisor.service"]).await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging();

    let bv_root = bv_root();
    let _lock_file = bv_utils::lock_file::LockFile::lock(&bv_root.join(INSTALL_PATH), ".lock")
        .with_context(|| "another BV installer instance is already running")?;
    let installer = Installer::new(SysTimer, SystemCtl, &bv_root).await?;
    if let Err(err) = installer.run().await {
        error!("{err:#}");
        Err(err)
    } else {
        Ok(())
    }
}
