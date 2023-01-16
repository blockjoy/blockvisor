use async_trait::async_trait;
use babel::{babelsup_service, utils};
use babel::{config, logging, run_flag::RunFlag, supervisor};
use babel_api::config::SupervisorConfig;
use eyre::{anyhow, Context};
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::fs::DirBuilder;
use tokio::sync::{oneshot, watch};
use tonic::transport::Server;

const DATA_DRIVE_PATH: &str = "/dev/vdb";
const VSOCK_HOST_CID: u32 = 3;
const VSOCK_SUPERVISOR_PORT: u32 = 41;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    logging::setup_logging()?;

    let cfg = config::load(&babel::env::BABEL_CONFIG_PATH).await?;

    let data_dir = &cfg.supervisor.data_directory_mount_point;
    tracing::info!("Recursively creating data directory at {data_dir}");
    DirBuilder::new().recursive(true).create(&data_dir).await?;
    tracing::info!("Mounting data directory at {data_dir}");
    // We assume that root drive will become /dev/vda, and data drive will become /dev/vdb inside VM
    // However, this can be a wrong assumption ¯\_(ツ)_/¯:
    // https://github.com/firecracker-microvm/firecracker-containerd/blob/main/docs/design-approaches.md#block-devices
    let output = tokio::process::Command::new("mount")
        .args([DATA_DRIVE_PATH, data_dir])
        .output()
        .await?;
    tracing::debug!("Mounted data directory: {output:?}");

    let mut run = RunFlag::run_until_ctrlc();

    let (babel_change_tx, babel_change_rx) =
        watch::channel(utils::file_checksum(&babel::env::BABEL_BIN_PATH).await.ok());
    let (sup_setup_tx, sup_setup_rx) = oneshot::channel();
    let sup_setup_tx = if let Ok(cfg) = load_config().await {
        sup_setup_tx
            .send(supervisor::SupervisorSetup::new(cfg))
            .map_err(|_| anyhow!("failed to setup supervisor"))?;
        None
    } else {
        Some(sup_setup_tx)
    };

    let supervisor_handle = tokio::spawn(supervisor::run::<SysTimer>(
        run.clone(),
        babel::env::BABEL_BIN_PATH.clone(),
        sup_setup_rx,
        babel_change_rx,
    ));
    let res = serve(run.clone(), sup_setup_tx, babel_change_tx).await;
    if run.load() {
        // make sure to stop supervisor gracefully
        // in case of abnormal server shutdown
        run.stop();
        supervisor_handle.await?;
    }
    res
}

async fn load_config() -> eyre::Result<SupervisorConfig> {
    tracing::info!(
        "Loading supervisor configuration at {}",
        babel::env::BABELSUP_CONFIG_PATH.to_string_lossy()
    );
    let toml_str = fs::read_to_string(babel::env::BABELSUP_CONFIG_PATH.as_path()).await?;
    supervisor::load_config(&toml_str)
}

struct SysTimer;

#[async_trait]
impl supervisor::Timer for SysTimer {
    fn now() -> Instant {
        Instant::now()
    }

    async fn sleep(duration: Duration) {
        tokio::time::sleep(duration).await
    }
}

#[cfg(target_os = "linux")]
async fn serve(
    mut run: RunFlag,
    sup_setup_tx: supervisor::SupervisorSetupTx,
    babel_change_tx: supervisor::BabelChangeTx,
) -> eyre::Result<()> {
    let babelsup_service = babel::babelsup_service::BabelSupService::new(
        sup_setup_tx,
        babel_change_tx,
        babel::env::BABEL_BIN_PATH.clone(),
        babel::env::BABELSUP_CONFIG_PATH.clone(),
    );
    let listener = tokio_vsock::VsockListener::bind(VSOCK_HOST_CID, VSOCK_SUPERVISOR_PORT)
        .with_context(|| "failed to bind to vsock")?;

    Server::builder()
        .max_concurrent_streams(2)
        .add_service(babelsup_service::pb::babel_sup_server::BabelSupServer::new(
            babelsup_service,
        ))
        .serve_with_incoming_shutdown(listener.incoming(), run.wait())
        .await?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn serve(
    _run: RunFlag,
    _sup_setup_tx: supervisor::SupervisorSetupTx,
    _babel_change_tx: supervisor::BabelChangeTx,
) -> eyre::Result<()> {
    unimplemented!()
}
