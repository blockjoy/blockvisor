use bv_utils::logging::setup_logging;
use std::env;
use tracing::info;

#[tokio::main(flavor = "multi_thread", worker_threads = 3)]
async fn main() -> eyre::Result<()> {
    setup_logging();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    babel::babel::run(babel::chroot_platform::Pal).await
}
