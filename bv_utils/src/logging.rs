use eyre::Result;
use tracing_subscriber::{
    self, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry,
};

pub fn setup_logging() -> Result<()> {
    tracing_subscriber::registry()
        .with(<tracing_journald::Layer as tracing_subscriber::Layer<
            Registry,
        >>::with_filter(
            tracing_journald::layer()?,
            EnvFilter::from_default_env(),
        ))
        .init();
    Ok(())
}
