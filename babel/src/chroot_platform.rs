use crate::{pal, BabelEngineClient};
use async_trait::async_trait;
use babel_api::babel::babel_server::Babel;
use babel_api::babel::NodeContext;
use babel_api::metadata::firewall::Config;
use babel_api::metadata::RamdiskConfiguration;
use bv_utils::cmd::{run_cmd, CmdError};
use bv_utils::rpc::RPC_REQUEST_TIMEOUT;
use bv_utils::run_flag::RunFlag;
use eyre::{anyhow, bail, Context};
use std::path::Path;
use tokio::fs;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::warn;

const BABEL_SOCKET: &str = "/babel.socket";
const ENGINE_SOCKET: &str = "/engine.socket";

pub struct UdsServer;

#[async_trait]
impl pal::BabelServer for UdsServer {
    async fn serve<T: Babel>(
        &self,
        server: babel_api::babel::babel_server::BabelServer<T>,
        mut run: RunFlag,
    ) -> eyre::Result<()> {
        let _ = fs::remove_file(BABEL_SOCKET).await;
        let uds_stream = UnixListenerStream::new(
            tokio::net::UnixListener::bind(BABEL_SOCKET)
                .with_context(|| "failed to bind to vsock")?,
        );
        Server::builder()
            .max_concurrent_streams(2)
            .add_service(server)
            .serve_with_incoming_shutdown(uds_stream, run.wait())
            .await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct UdsConnector;

impl pal::BabelEngineConnector for UdsConnector {
    fn connect(&self) -> BabelEngineClient {
        babel_api::babel::babel_engine_client::BabelEngineClient::with_interceptor(
            bv_utils::rpc::build_socket_channel(ENGINE_SOCKET),
            bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
        )
    }
}

pub struct Pal;

#[async_trait]
impl pal::BabelPal for Pal {
    type BabelServer = UdsServer;
    fn babel_server(&self) -> Self::BabelServer {
        UdsServer
    }

    type Connector = UdsConnector;
    fn connector(&self) -> Self::Connector {
        UdsConnector
    }

    async fn mount_data_drive(&self, _data_directory_mount_point: &str) -> eyre::Result<()> {
        Ok(())
    }

    async fn umount_data_drive(
        &self,
        _data_directory_mount_point: &str,
        _fuser_kill: bool,
    ) -> eyre::Result<()> {
        Ok(())
    }

    async fn is_data_drive_mounted(&self, _data_directory_mount_point: &str) -> eyre::Result<bool> {
        Ok(true)
    }

    async fn set_node_context(&self, node_context: NodeContext) -> eyre::Result<()> {
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

    async fn set_swap_file(
        &self,
        _swap_size_mb: u64,
        _swap_file_location: &str,
    ) -> eyre::Result<()> {
        Ok(())
    }

    async fn is_swap_file_set(
        &self,
        _swap_size_mb: u64,
        _swap_file_location: &str,
    ) -> eyre::Result<bool> {
        Ok(true)
    }

    async fn set_ram_disks(
        &self,
        ram_disks: Option<Vec<RamdiskConfiguration>>,
    ) -> eyre::Result<()> {
        let ram_disks = ram_disks.unwrap_or_default();
        let df_out = df_tmpfs().await?;
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
        let df_out = df_tmpfs().await?;
        Ok(ram_disks
            .iter()
            .all(|disk| df_out.contains(disk.ram_disk_mount_point.trim_end_matches('/'))))
    }

    async fn apply_firewall_config(&self, _config: Config) -> eyre::Result<()> {
        Ok(())
    }
}

async fn df_tmpfs() -> eyre::Result<String> {
    match run_cmd("df", ["-t", "tmpfs", "--output=target"]).await {
        Ok(stdout) => Ok(stdout),
        Err(CmdError::Failed { cmd, code, stderr }) => {
            if code == 1 && stderr.contains("no file systems processed") {
                Ok(Default::default())
            } else {
                bail!("cant check mounted ramdisks with df: {cmd} return {code}: {stderr}")
            }
        }
        Err(err) => {
            bail!("cant check mounted ramdisks with df: {err:#}")
        }
    }
}
