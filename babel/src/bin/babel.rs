use async_trait::async_trait;
use babel::{
    babel_service,
    babel_service::{BabelSetup, MountDataDrive, MountError},
    jobs::JOBS_DIR,
    jobs_manager, logging,
    logs_service::LogsService,
    utils, BABEL_LOGS_UDS_PATH,
};
use babel_api::config::BabelConfig;
use bv_utils::run_flag::RunFlag;
use eyre::{anyhow, bail, Context};
use std::{path::Path, sync::Arc};
use tokio::{
    fs,
    sync::{broadcast, oneshot, RwLock},
};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

lazy_static::lazy_static! {
    static ref JOB_RUNNER_BIN_PATH: &'static Path = Path::new("/usr/bin/babel_job_runner");
    static ref BABEL_CONFIG_PATH: &'static Path = Path::new("/etc/babel.conf");
}
const DATA_DRIVE_PATH: &str = "/dev/vdb";
const VSOCK_HOST_CID: u32 = 3;
const VSOCK_BABEL_PORT: u32 = 42;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    logging::setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let vsock_listener = tokio_vsock::VsockListener::bind(VSOCK_HOST_CID, VSOCK_BABEL_PORT)
        .with_context(|| "failed to bind to vsock")?;

    let job_runner_lock = Arc::new(RwLock::new(
        utils::file_checksum(&JOB_RUNNER_BIN_PATH).await.ok(),
    ));

    let (client, manager) =
        jobs_manager::create(&JOBS_DIR, job_runner_lock.clone(), &JOB_RUNNER_BIN_PATH)?;

    let mnt = Mnt;
    let (logs_tx, logs_rx) = oneshot::channel();
    let setup = if let Ok(config) = load_config().await {
        match mnt
            .mount_data_drive(&config.data_directory_mount_point)
            .await
        {
            // it is ok if data drive is already mounted - it means that babel was restarted e.g. during self-update
            Ok(()) | Err(MountError::AlreadyMounted { .. }) => {}
            Err(err) => bail!(err),
        }
        let (logs_broadcast_tx, logs_rx) = broadcast::channel(config.log_buffer_capacity_ln);
        logs_tx
            .send(logs_broadcast_tx)
            .map_err(|_| anyhow!("failed to setup logs_server"))?;
        BabelSetup::Ready(logs_rx)
    } else {
        BabelSetup::Uninitialized(logs_tx)
    };

    let babel_service = babel_service::BabelService::new(
        job_runner_lock,
        JOB_RUNNER_BIN_PATH.to_path_buf(),
        client,
        BABEL_CONFIG_PATH.to_path_buf(),
        mnt,
        setup,
    )
    .await?;

    let mut run = RunFlag::run_until_ctrlc();
    let manager_handle = tokio::spawn(manager.run(run.clone()));
    let logs_run = run.clone();
    let log_service_handle = tokio::spawn(async move {
        if let Some(logs_service) = LogsService::wait_for_logs_tx(logs_rx).await {
            serve_logs(logs_run, logs_service).await
        } else {
            Ok(())
        }
    });

    let res = Server::builder()
        .max_concurrent_streams(2)
        .add_service(babel_api::babel_server::BabelServer::new(babel_service))
        .serve_with_incoming_shutdown(vsock_listener.incoming(), run.wait())
        .await;
    if run.load() {
        // make sure to stop manager gracefully
        // in case of abnormal server shutdown
        run.stop();
        manager_handle.await?;
        let _ = log_service_handle.await;
    }
    Ok(res?)
}

struct Mnt;

#[async_trait]
impl babel_service::MountDataDrive for Mnt {
    async fn mount_data_drive(
        &self,
        data_directory_mount_point: &str,
    ) -> eyre::Result<(), MountError> {
        // We assume that root drive will become /dev/vda, and data drive will become /dev/vdb inside VM
        // However, this can be a wrong assumption ¯\_(ツ)_/¯:
        // https://github.com/firecracker-microvm/firecracker-containerd/blob/main/docs/design-approaches.md#block-devices
        let out = utils::mount_drive(DATA_DRIVE_PATH, data_directory_mount_point)
            .await
            .map_err(|err| MountError::Internal {
                data_drive_path: DATA_DRIVE_PATH.to_string(),
                data_directory_mount_point: data_directory_mount_point.to_string(),
                err,
            })?;
        match out.status.code() {
            Some(0) => Ok(()),
            Some(32) if String::from_utf8_lossy(&out.stderr).contains("already mounted") => {
                Err(MountError::AlreadyMounted {
                    data_drive_path: DATA_DRIVE_PATH.to_string(),
                    data_directory_mount_point: data_directory_mount_point.to_string(),
                })
            }
            _ => Err(MountError::MountFailed {
                data_drive_path: DATA_DRIVE_PATH.to_string(),
                data_directory_mount_point: data_directory_mount_point.to_string(),
                out,
            }),
        }?;
        Ok(())
    }
}

async fn serve_logs(mut run: RunFlag, logs_service: LogsService) -> eyre::Result<()> {
    let _ = fs::remove_file(*BABEL_LOGS_UDS_PATH).await;
    let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(*BABEL_LOGS_UDS_PATH)?);

    Server::builder()
        .add_service(babel_api::logs_collector_server::LogsCollectorServer::new(
            logs_service,
        ))
        .serve_with_incoming_shutdown(uds_stream, run.wait())
        .await?;
    Ok(())
}

async fn load_config() -> eyre::Result<BabelConfig> {
    info!(
        "Loading babel configuration at {}",
        BABEL_CONFIG_PATH.to_string_lossy()
    );
    Ok(serde_json::from_str::<BabelConfig>(
        &fs::read_to_string(*BABEL_CONFIG_PATH).await?,
    )?)
}
