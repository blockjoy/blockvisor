use async_trait::async_trait;
use babel::{babelsup_service, utils};
use babel::{config, logging, run_flag::RunFlag, supervisor};
use eyre::Context;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::fs::DirBuilder;
use tokio::sync::{broadcast, watch};
use tonic::transport::Server;

const DATA_DRIVE_PATH: &str = "/dev/vdb";
const VSOCK_HOST_CID: u32 = 3;
const VSOCK_SUPERVISOR_PORT: u32 = 41;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    logging::setup_logging()?;

    let cfg = config::load(&babel::env::BABEL_CONFIG_PATH).await?;

    let data_dir = &cfg.config.data_directory_mount_point;
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

    let supervisor = supervisor::Supervisor::<SysTimer>::new(
        run.clone(),
        &fs::read_to_string(babel::env::PROC_CMDLINE_PATH.as_path()).await?,
        cfg.supervisor,
        babel::env::BABEL_BIN_PATH.clone(),
        babel_change_rx,
    )?;
    let logs_rx = supervisor.get_logs_rx();
    let supervisor_handle = tokio::spawn(supervisor.run());
    let res = serve(run.clone(), logs_rx, babel_change_tx).await;
    if run.load() {
        // make sure to stop supervisor gracefully
        // in case of abnormal server shutdown
        run.stop();
        supervisor_handle.await?;
    }
    res
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
    logs_rx: broadcast::Receiver<String>,
    babel_change_tx: watch::Sender<Option<u32>>,
) -> eyre::Result<()> {
    let babelsup_service = babel::babelsup_service::BabelSupService::new(
        logs_rx,
        babel_change_tx,
        babel::env::BABEL_BIN_PATH.clone(),
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
    _logs_rx: broadcast::Receiver<String>,
    _babel_change_tx: watch::Sender<Option<u32>>,
) -> eyre::Result<()> {
    unimplemented!()
}
