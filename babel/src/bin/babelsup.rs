use async_trait::async_trait;
use babel::babelsup_service::SupervisorSetup;
use babel::{babelsup_service, utils};
use babel::{logging, run_flag::RunFlag, supervisor};
use babel_api::config::SupervisorConfig;
use eyre::{anyhow, Context};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::fs::DirBuilder;
use tokio::sync::{oneshot, watch};
use tonic::transport::Server;
use tracing::info;

lazy_static::lazy_static! {
    static ref BABELSUP_CONFIG_PATH: &'static Path = Path::new("/etc/babelsup.conf");
    static ref BABEL_BIN_PATH: &'static Path = Path::new("/usr/bin/babel");
}
const DATA_DRIVE_PATH: &str = "/dev/vdb";
const VSOCK_HOST_CID: u32 = 3;
const VSOCK_SUPERVISOR_PORT: u32 = 41;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    logging::setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let mut run = RunFlag::run_until_ctrlc();

    let (babel_change_tx, babel_change_rx) =
        watch::channel(utils::file_checksum(&BABEL_BIN_PATH).await.ok());
    let (sup_setup_tx, sup_setup_rx) = oneshot::channel();
    let sup_setup = if let Ok(cfg) = load_config().await {
        mount_data_drive(&cfg.data_directory_mount_point).await?;
        let setup = supervisor::SupervisorSetup::new(cfg);
        let logs_rx = setup.log_buffer.subscribe();
        sup_setup_tx
            .send(setup)
            .map_err(|_| anyhow!("failed to setup supervisor"))?;
        SupervisorSetup::LogsRx(logs_rx)
    } else {
        SupervisorSetup::SetupTx(sup_setup_tx)
    };

    let supervisor_handle = tokio::spawn(supervisor::run(
        SysTimer,
        run.clone(),
        BABEL_BIN_PATH.to_path_buf(),
        sup_setup_rx,
        babel_change_rx,
    ));
    let res = serve(run.clone(), sup_setup, babel_change_tx).await;
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

struct SysTimer;

#[async_trait]
impl supervisor::Timer for SysTimer {
    fn now(&self) -> Instant {
        Instant::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await
    }
}

struct ConfigObserver;

#[async_trait]
impl babelsup_service::SupervisorConfigObserver for ConfigObserver {
    async fn supervisor_config_set(&self, cfg: &SupervisorConfig) -> eyre::Result<()> {
        mount_data_drive(&cfg.data_directory_mount_point).await
    }
}

async fn mount_data_drive(data_dir: &str) -> eyre::Result<()> {
    info!("Recursively creating data directory at {data_dir}");
    DirBuilder::new().recursive(true).create(&data_dir).await?;
    info!("Mounting data directory at {data_dir}");
    // We assume that root drive will become /dev/vda, and data drive will become /dev/vdb inside VM
    // However, this can be a wrong assumption ¯\_(ツ)_/¯:
    // https://github.com/firecracker-microvm/firecracker-containerd/blob/main/docs/design-approaches.md#block-devices
    let output = tokio::process::Command::new("mount")
        .args([DATA_DRIVE_PATH, data_dir])
        .output()
        .await
        .with_context(|| "failed to mount data drive".to_string())?;
    tracing::debug!("Mounted data directory: {output:?}");
    Ok(())
}

async fn serve(
    mut run: RunFlag,
    sup_setup: SupervisorSetup,
    babel_change_tx: supervisor::BabelChangeTx,
) -> eyre::Result<()> {
    let babelsup_service = babelsup_service::BabelSupService::new(
        sup_setup,
        babel_change_tx,
        BABEL_BIN_PATH.to_path_buf(),
        BABELSUP_CONFIG_PATH.to_path_buf(),
        ConfigObserver,
    );
    let listener = tokio_vsock::VsockListener::bind(VSOCK_HOST_CID, VSOCK_SUPERVISOR_PORT)
        .with_context(|| "failed to bind to vsock")?;

    Server::builder()
        .max_concurrent_streams(2)
        .add_service(babel_api::babel_sup_server::BabelSupServer::new(
            babelsup_service,
        ))
        .serve_with_incoming_shutdown(listener.incoming(), run.wait())
        .await?;
    Ok(())
}
