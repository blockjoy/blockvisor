use bv_utils::logging::setup_logging;
use std::{env, os::unix::fs};
use tracing::info;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> eyre::Result<()> {
    setup_logging();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let mut args = env::args();
    let arg = args.nth(1).expect("missing --chroot argument");
    if arg == "--chroot" {
        let chroot_dir = args.next().expect("missing chroot directory");
        fs::chroot(chroot_dir)?;
        env::set_current_dir("/")?;
    } else {
        panic!("missing --chroot argument")
    }
    babel::babel::run(babel::chroot_platform::Pal).await
}
