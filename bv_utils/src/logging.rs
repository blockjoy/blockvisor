use tracing_subscriber::{
    self, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

pub fn setup_logging() {
    if let Ok(journald) = tracing_journald::layer() {
        let _ = tracing_subscriber::registry()
            .with(<tracing_journald::Layer as Layer<Registry>>::with_filter(
                journald.with_syslog_identifier("blockvisor".to_string()),
                EnvFilter::from_default_env(),
            ))
            .try_init();
    }
}
