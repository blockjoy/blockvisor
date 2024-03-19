use blockvisord::{
    blockvisord::BlockvisorD,
    linux_fc_platform::bv_root,
    pal::Pal,
    {config, load_pal},
};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    let run = RunFlag::run_until_ctrlc();
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let pal = load_pal!(&bv_root())?;
    let config = config::Config::load(pal.bv_root()).await?;
    BlockvisorD::new(pal, config).await?.run(run).await?;
    Ok(())
}
