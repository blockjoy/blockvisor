use anyhow::Result;
use blockvisord::installer;
use blockvisord::installer::Installer;
use blockvisord::linux_platform::bv_root;
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

#[tokio::main]
async fn main() -> Result<()> {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .finish()
        .with(tracing_journald::layer()?)
        .init();

    let res = Installer::new(SysTimer, &bv_root()).run().await;
    if let Err(err) = res {
        error!("{err}");
        Err(err)
    } else {
        Ok(())
    }
}
