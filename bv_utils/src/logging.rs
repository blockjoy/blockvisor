use tracing_subscriber::{
    self, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

#[cfg(not(feature = "bv_fmt_log"))]
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

#[cfg(feature = "bv_fmt_log")]
pub fn setup_logging() {
    let fmt = <tracing_subscriber::fmt::Layer<Registry> as Layer<Registry>>::with_filter(
        tracing_subscriber::fmt::layer(),
        EnvFilter::from_default_env(),
    );
    let _ = tracing_subscriber::registry().with(fmt).try_init();
}
