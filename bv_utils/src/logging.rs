use tracing_subscriber::{
    self, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

pub fn setup_logging() {
    let fmt = <tracing_subscriber::fmt::Layer<Registry> as Layer<Registry>>::with_filter(
        tracing_subscriber::fmt::layer(),
        EnvFilter::from_default_env(),
    );
    let _ = if let Ok(journald) = tracing_journald::layer() {
        tracing_subscriber::registry()
            .with(
                fmt.and_then(<tracing_journald::Layer as Layer<Registry>>::with_filter(
                    journald,
                    EnvFilter::from_default_env(),
                )),
            )
            .try_init()
    } else {
        tracing_subscriber::registry().with(fmt).try_init()
    };
}
