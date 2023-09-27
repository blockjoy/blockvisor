use eyre::Result;
use tracing_subscriber::{
    self, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, FmtSubscriber,
};

pub fn setup_logging() {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .finish()
        .init();
}

pub fn setup_logging_with_journald() -> Result<()> {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .finish()
        .with(tracing_journald::layer()?)
        .init();

    Ok(())
}
