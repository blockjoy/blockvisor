use crate::{pal, BabelEngineClient};
use async_trait::async_trait;
use babel_api::babel::babel_server::Babel;
use babel_api::babel::NodeContext;
use babel_api::metadata::firewall::Config;
use babel_api::metadata::RamdiskConfiguration;
use bv_utils::cmd::run_cmd;
use bv_utils::rpc::{RPC_CONNECT_TIMEOUT, RPC_REQUEST_TIMEOUT};
use bv_utils::run_flag::RunFlag;
use eyre::{anyhow, Context};
use std::path::Path;
use tokio::fs;
use tonic::transport::{Endpoint, Server, Uri};
use tracing::warn;

const DATA_DRIVE_PATH: &str = "/dev/vdb";
const VSOCK_GUEST_CID: u32 = 3;
const VSOCK_BABEL_PORT: u32 = 42;
const VSOCK_HOST_CID: u32 = 2;
const VSOCK_ENGINE_PORT: u32 = 40;

pub struct VSockServer;

#[async_trait]
impl pal::BabelServer for VSockServer {
    async fn serve<T: Babel>(
        &self,
        server: babel_api::babel::babel_server::BabelServer<T>,
        mut run: RunFlag,
    ) -> eyre::Result<()> {
        let incoming = tokio_vsock::VsockListener::bind(VSOCK_GUEST_CID, VSOCK_BABEL_PORT)
            .with_context(|| "failed to bind to vsock")?
            .incoming();
        Server::builder()
            .max_concurrent_streams(2)
            .add_service(server)
            .serve_with_incoming_shutdown(incoming, run.wait())
            .await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct VSockConnector;

impl pal::BabelEngineConnector for VSockConnector {
    fn connect(&self) -> BabelEngineClient {
        babel_api::babel::babel_engine_client::BabelEngineClient::with_interceptor(
            Endpoint::from_static("http://[::]:50052")
                .connect_timeout(RPC_CONNECT_TIMEOUT)
                .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                    tokio_vsock::VsockStream::connect(VSOCK_HOST_CID, VSOCK_ENGINE_PORT)
                })),
            bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
        )
    }
}

pub struct Pal;

#[async_trait]
impl pal::BabelPal for Pal {
    type BabelServer = VSockServer;
    fn babel_server(&self) -> Self::BabelServer {
        VSockServer
    }

    type Connector = VSockConnector;
    fn connector(&self) -> Self::Connector {
        VSockConnector
    }

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
                let _ = run_cmd("fuser", ["-km", data_directory_mount_point]).await;
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
        if let Err(err) = fs::write(crate::NODE_ENV_FILE_PATH, node_env).await {
            warn!("failed to write node_env file: {err:#}");
        }
        if Path::new(crate::POST_SETUP_SCRIPT).exists() {
            run_cmd::<[&str; 0], _>(crate::POST_SETUP_SCRIPT, []).await?;
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

    async fn apply_firewall_config(&self, config: Config) -> eyre::Result<()> {
        crate::ufw_wrapper::apply_firewall_config(config).await
    }
}
