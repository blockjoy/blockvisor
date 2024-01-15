use babel::babelsup_service::SupervisorStatus;
use babel::{babelsup_service, supervisor, utils};
use babel_api::babelsup::SupervisorConfig;
use bv_utils::logging::setup_logging;
use bv_utils::run_flag::RunFlag;
use eyre::{anyhow, Context};
use std::path::Path;
use tokio::fs;
use tokio::sync::{oneshot, watch};
use tonic::transport::Server;
use tracing::info;

lazy_static::lazy_static! {
    static ref BABELSUP_CONFIG_PATH: &'static Path = Path::new("/etc/babelsup.conf");
    static ref BABEL_BIN_PATH: &'static Path = Path::new("/usr/bin/babel");
}
const VSOCK_HOST_CID: u32 = 3;
const VSOCK_SUPERVISOR_PORT: u32 = 41;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let mut run = RunFlag::run_until_ctrlc();

    let (babel_change_tx, babel_change_rx) =
        watch::channel(utils::file_checksum(&BABEL_BIN_PATH).await.ok());
    let (sup_config_tx, sup_config_rx) = oneshot::channel();
    let sup_status = if let Ok(cfg) = load_config().await {
        sup_config_tx
            .send(cfg)
            .map_err(|_| anyhow!("failed to setup supervisor"))?;
        SupervisorStatus::Ready
    } else {
        SupervisorStatus::Uninitialized(sup_config_tx)
    };

    let supervisor_handle = tokio::spawn(supervisor::run(
        bv_utils::timer::SysTimer,
        run.clone(),
        BABEL_BIN_PATH.to_path_buf(),
        sup_config_rx,
        babel_change_rx,
    ));
    let res = serve(run.clone(), sup_status, babel_change_tx).await;
    if run.load() {
        // make sure to stop supervisor gracefully
        // in case of abnormal server shutdown
        run.stop();
        supervisor_handle.await?;
    }
    res
}

async fn load_config() -> eyre::Result<SupervisorConfig> {
    info!(
        "Loading supervisor configuration at {}",
        BABELSUP_CONFIG_PATH.to_string_lossy()
    );
    let json_str = fs::read_to_string(&*BABELSUP_CONFIG_PATH).await?;
    supervisor::load_config(&json_str)
}

async fn serve(
    mut run: RunFlag,
    sup_status: SupervisorStatus,
    babel_change_tx: supervisor::BabelChangeTx,
) -> eyre::Result<()> {
    let babelsup_service = babelsup_service::BabelSupService::new(
        sup_status,
        babel_change_tx,
        BABEL_BIN_PATH.to_path_buf(),
        BABELSUP_CONFIG_PATH.to_path_buf(),
    );
    let listener = tokio_vsock::VsockListener::bind(VSOCK_HOST_CID, VSOCK_SUPERVISOR_PORT)
        .with_context(|| "failed to bind to vsock")?;

    Server::builder()
        .max_concurrent_streams(2)
        .add_service(babel_api::babelsup::babel_sup_server::BabelSupServer::new(
            babelsup_service,
        ))
        .serve_with_incoming_shutdown(listener.incoming(), run.wait())
        .await?;
    Ok(())
}
