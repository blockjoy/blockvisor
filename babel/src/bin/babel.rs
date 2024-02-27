use async_trait::async_trait;
use babel::{
    babel_service, babel_service::BabelServiceState, is_babel_config_applied, jobs::JOBS_DIR,
    jobs_manager, jobs_manager::JobsManagerState, load_config, logs_service::LogsService, utils,
    BabelPal, VSockConnector, BABEL_LOGS_UDS_PATH, JOBS_MONITOR_UDS_PATH,
};
use babel_api::babel::NodeContext;
use babel_api::metadata::RamdiskConfiguration;
use bv_utils::{cmd::run_cmd, logging::setup_logging, run_flag::RunFlag};
use eyre::{anyhow, Context};
use std::{path::Path, sync::Arc};
use tokio::{
    fs,
    sync::{broadcast, oneshot, RwLock},
};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::{info, warn};

lazy_static::lazy_static! {
    static ref JOB_RUNNER_BIN_PATH: &'static Path = Path::new("/usr/bin/babel_job_runner");
    static ref BABEL_CONFIG_PATH: &'static Path = Path::new("/etc/babel.conf");
}
const DATA_DRIVE_PATH: &str = "/dev/vdb";
const VSOCK_GUEST_CID: u32 = 3;
const VSOCK_BABEL_PORT: u32 = 42;
const NODE_ENV_FILE_PATH: &str = "/var/lib/babel/node_env";
const POST_SETUP_SCRIPT: &str = "/var/lib/babel/post_setup.sh";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> eyre::Result<()> {
    setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let vsock_listener = tokio_vsock::VsockListener::bind(VSOCK_GUEST_CID, VSOCK_BABEL_PORT)
        .with_context(|| "failed to bind to vsock")?;

    let job_runner_lock = Arc::new(RwLock::new(
        utils::file_checksum(&JOB_RUNNER_BIN_PATH).await.ok(),
    ));

    let pal = Pal;
    let (logs_tx, logs_rx) = oneshot::channel();
    let (service_state, jobs_manager_state) =
        if let Ok(config) = load_config(&BABEL_CONFIG_PATH).await {
            let (logs_broadcast_tx, logs_rx) = broadcast::channel(config.log_buffer_capacity_ln);
            logs_tx
                .send(logs_broadcast_tx)
                .map_err(|_| anyhow!("failed to setup logs_server"))?;
            (
                BabelServiceState::Ready(logs_rx),
                if is_babel_config_applied(&pal, &config).await? {
                    JobsManagerState::Ready
                } else {
                    JobsManagerState::NotReady
                },
            )
        } else {
            (
                BabelServiceState::NotReady(logs_tx),
                JobsManagerState::NotReady,
            )
        };

    let (client, monitor, manager) = jobs_manager::create(
        babel::VSockConnector,
        &JOBS_DIR,
        job_runner_lock.clone(),
        &JOB_RUNNER_BIN_PATH,
        jobs_manager_state,
    )
    .await?;
    let babel_service = babel_service::BabelService::new(
        job_runner_lock,
        JOB_RUNNER_BIN_PATH.to_path_buf(),
        client,
        BABEL_CONFIG_PATH.to_path_buf(),
        pal,
        service_state,
    )?;

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

    let monitor_run = run.clone();
    let (res, _) = tokio::join!(
        Server::builder()
            .max_concurrent_streams(2)
            .add_service(babel_api::babel::babel_server::BabelServer::new(
                babel_service,
            ))
            .serve_with_incoming_shutdown(vsock_listener.incoming(), run.wait()),
        serve_jobs_monitor(monitor_run, monitor)
    );
    if run.load() {
        // make sure to stop manager gracefully
        // in case of abnormal server shutdown
        run.stop();
        manager_handle.await?;
        let _ = log_service_handle.await;
    }
    Ok(res?)
}

struct Pal;

#[async_trait]
impl BabelPal for Pal {
    async fn mount_data_drive(&self, data_directory_mount_point: &str) -> eyre::Result<()> {
        if !self
            .is_data_drive_mounted(data_directory_mount_point)
            .await?
        {
            // We assume that root drive will become /dev/vda, and data drive will become /dev/vdb inside VM
            // However, this can be a wrong assumption ¯\_(ツ)_/¯:
            // https://github.com/firecracker-microvm/firecracker-containerd/blob/main/docs/design-approaches.md#block-devices
            fs::create_dir_all(data_directory_mount_point).await?;
            run_cmd("mount", [DATA_DRIVE_PATH, data_directory_mount_point])
                .await
                .map_err(|err| {
                    anyhow!(
                    "failed to mount {DATA_DRIVE_PATH} into {data_directory_mount_point}: {err:#}"
                )
                })?;
        }
        Ok(())
    }

    async fn umount_data_drive(
        &self,
        data_directory_mount_point: &str,
        fuser_kill: bool,
    ) -> eyre::Result<()> {
        if self
            .is_data_drive_mounted(data_directory_mount_point)
            .await?
        {
            if fuser_kill {
                if let Err(err) = run_cmd("fuser", ["-km", data_directory_mount_point]).await {
                    warn!(
                        "failed to 'fuser -km {data_directory_mount_point}' before umount: {err:#}"
                    );
                }
            }
            run_cmd("umount", [data_directory_mount_point])
                .await
                .map_err(|err| anyhow!("failed to umount {data_directory_mount_point}: {err:#}"))?;
        }
        Ok(())
    }

    async fn is_data_drive_mounted(&self, data_directory_mount_point: &str) -> eyre::Result<bool> {
        let df_out = run_cmd("df", ["--output=target"])
            .await
            .map_err(|err| anyhow!("can't check if data drive is mounted, df: {err:#}"))?;
        Ok(df_out.contains(data_directory_mount_point.trim_end_matches('/')))
    }

    async fn set_node_context(&self, node_context: NodeContext) -> eyre::Result<()> {
        run_cmd(
            "hostnamectl",
            ["set-hostname", &node_context.node_name.replace('_', "-")],
        )
        .await
        .map_err(|err| anyhow!("hostnamectl error: {err:#}"))?;

        let node_env = format!(
            "BV_HOST_ID={}\n\
             BV_HOST_NAME={}\n\
             BV_API_URL={}\n\
             NODE_ID={}\n\
             NODE_NAME={}\n\
             NODE_TYPE={}\n\
             BLOCKCHAIN_TYPE={}\n\
             NODE_VERSION={}\n\
             NODE_IP={}\n\
             NODE_GATEWAY={}\n\
             NODE_STANDALONE={}\n",
            node_context.bv_id,
            node_context.bv_name,
            node_context.bv_api_url,
            node_context.node_id,
            node_context.node_name,
            node_context.node_type,
            node_context.protocol,
            node_context.node_version,
            node_context.ip,
            node_context.gateway,
            node_context.standalone
        );
        if let Err(err) = fs::write(NODE_ENV_FILE_PATH, node_env).await {
            warn!("failed to write node_env file: {err:#}");
        }
        if Path::new(POST_SETUP_SCRIPT).exists() {
            run_cmd::<[&str; 0], _>(POST_SETUP_SCRIPT, []).await?;
        }
        Ok(())
    }

    /// Set a swap file inside VM
    ///
    /// Swap file location is `/swapfile`. If swap file exists, it will be turned off and recreated
    ///
    /// Based on this tutorial:
    /// https://www.digitalocean.com/community/tutorials/how-to-add-swap-space-on-ubuntu-20-04
    async fn set_swap_file(&self, swap_size_mb: u64, swap_file_location: &str) -> eyre::Result<()> {
        let swappiness = 1;
        let pressure = 50;
        let _ = run_cmd("swapoff", [swap_file_location]).await;
        let _ = tokio::fs::remove_file(swap_file_location).await;
        run_cmd(
            "fallocate",
            ["-l", &format!("{swap_size_mb}MB"), swap_file_location],
        )
        .await
        .map_err(|err| anyhow!("fallocate error: {err:#}"))?;
        run_cmd("chmod", ["600", swap_file_location])
            .await
            .map_err(|err| anyhow!("chmod error: {err:#}"))?;
        run_cmd("mkswap", [swap_file_location])
            .await
            .map_err(|err| anyhow!("mkswap error: {err:#}"))?;
        run_cmd("swapon", [swap_file_location])
            .await
            .map_err(|err| anyhow!("swapon error: {err:#}"))?;
        run_cmd("sysctl", [&format!("vm.swappiness={swappiness}")])
            .await
            .map_err(|err| anyhow!("sysctl error: {err:#}"))?;
        run_cmd("sysctl", [&format!("vm.vfs_cache_pressure={pressure}")])
            .await
            .map_err(|err| anyhow!("sysctl error: {err:#}"))?;
        Ok(())
    }

    async fn is_swap_file_set(
        &self,
        _swap_size_mb: u64,
        swap_file_location: &str,
    ) -> eyre::Result<bool> {
        let path = Path::new(swap_file_location);
        Ok(path.exists())
    }

    /// Set RAM disks inside VM
    ///
    /// Should be doing something like that
    /// > mkdir -p /mnt/ramdisk
    /// > mount -t tmpfs -o rw,size=512M tmpfs /mnt/ramdisk
    async fn set_ram_disks(
        &self,
        ram_disks: Option<Vec<RamdiskConfiguration>>,
    ) -> eyre::Result<()> {
        let ram_disks = ram_disks.unwrap_or_default();
        let df_out = run_cmd("df", ["-t", "tmpfs", "--output=target"])
            .await
            .map_err(|err| anyhow!("cant check mounted ramdisks with df: {err:#}"))?;
        for disk in ram_disks {
            if df_out.contains(&disk.ram_disk_mount_point) {
                continue;
            }
            run_cmd("mkdir", ["-p", &disk.ram_disk_mount_point])
                .await
                .map_err(|err| anyhow!("mkdir error: {err:#}"))?;
            run_cmd(
                "mount",
                [
                    "-t",
                    "tmpfs",
                    "-o",
                    &format!("rw,size={}M", disk.ram_disk_size_mb),
                    "tmpfs",
                    &disk.ram_disk_mount_point,
                ],
            )
            .await
            .map_err(|err| anyhow!("mount error: {err:#}"))?;
        }
        Ok(())
    }

    async fn is_ram_disks_set(
        &self,
        ram_disks: Option<Vec<RamdiskConfiguration>>,
    ) -> eyre::Result<bool> {
        let ram_disks = ram_disks.unwrap_or_default();
        let df_out = run_cmd("df", ["-t", "tmpfs", "--output=target"])
            .await
            .map_err(|err| anyhow!("cant check mounted ramdisks with df: {err:#}"))?;
        Ok(ram_disks
            .iter()
            .all(|disk| df_out.contains(disk.ram_disk_mount_point.trim_end_matches('/'))))
    }
}

async fn serve_logs(mut run: RunFlag, logs_service: LogsService) -> eyre::Result<()> {
    let _ = fs::remove_file(BABEL_LOGS_UDS_PATH).await;
    let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(BABEL_LOGS_UDS_PATH)?);

    Server::builder()
        .add_service(
            babel_api::babel::logs_collector_server::LogsCollectorServer::new(logs_service),
        )
        .serve_with_incoming_shutdown(uds_stream, run.wait())
        .await?;
    Ok(())
}

async fn serve_jobs_monitor(
    mut run: RunFlag,
    jobs_monitor_service: jobs_manager::Monitor<VSockConnector>,
) -> eyre::Result<()> {
    let _ = fs::remove_file(JOBS_MONITOR_UDS_PATH).await;
    let uds_stream =
        UnixListenerStream::new(tokio::net::UnixListener::bind(JOBS_MONITOR_UDS_PATH)?);

    Server::builder()
        .add_service(
            babel_api::babel::jobs_monitor_server::JobsMonitorServer::new(jobs_monitor_service),
        )
        .serve_with_incoming_shutdown(uds_stream, run.wait())
        .await?;
    Ok(())
}
