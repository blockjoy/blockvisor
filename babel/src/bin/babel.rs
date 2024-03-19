use bv_utils::logging::setup_logging;
use std::{env, os::unix::fs};
use tracing::info;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> eyre::Result<()> {
    setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let mut args = env::args();
    if let Some(chroot_dir) = args.nth(1) {
        fs::chroot(chroot_dir)?;
        env::set_current_dir("/")?;
        babel::babel::run(babel::chroot_platform::Pal).await
    } else {
        babel::babel::run(babel::fc_platform::Pal).await
    }
}
