use anyhow::Result;
use async_trait::async_trait;
use blockvisord::installer;
use blockvisord::installer::Installer;
use blockvisord::linux_platform::bv_root;
use blockvisord::utils::run_cmd;
use std::thread::sleep;
use std::time::{Duration, Instant};
use tracing::error;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, FmtSubscriber};

struct SysTimer;

impl installer::Timer for SysTimer {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, duration: Duration) {
        sleep(duration)
    }
}

struct SystemCtl;

#[async_trait]
impl installer::BvService for SystemCtl {
    async fn reload(&self) -> Result<()> {
        run_cmd("systemctl", ["daemon-reload"]).await
    }

    async fn stop(&self) -> Result<()> {
        run_cmd("systemctl", ["stop", "blockvisor.service"]).await
    }

    async fn start(&self) -> Result<()> {
        run_cmd("systemctl", ["start", "blockvisor.service"]).await
    }

    async fn enable(&self) -> Result<()> {
        run_cmd("systemctl", ["enable", "blockvisor.service"]).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .finish()
        .with(tracing_journald::layer()?)
        .init();

    let res = Installer::new(SysTimer, SystemCtl, &bv_root())
        .await?
        .run()
        .await;
    if let Err(err) = res {
        error!("{err}");
        Err(err)
    } else {
        Ok(())
    }
}
