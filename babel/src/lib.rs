pub mod async_pid_watch;
pub mod babel_service;
pub mod babelsup_service;
pub mod checksum;
pub mod compression;
pub mod download_job;
pub mod job_runner;
pub mod jobs;
pub mod jobs_manager;
pub mod log_buffer;
pub mod logs_service;
pub mod run_sh_job;
pub mod supervisor;
pub mod ufw_wrapper;
pub mod upload_job;
pub mod utils;

use async_trait::async_trait;
use babel_api::metadata::{BabelConfig, RamdiskConfiguration};
use bv_utils::rpc::{RPC_CONNECT_TIMEOUT, RPC_REQUEST_TIMEOUT};
use eyre::{Context, Result};
use std::path::Path;
use tokio::fs;
use tonic::{
    codegen::InterceptedService,
    transport::{Channel, Endpoint, Uri},
};
use tracing::info;

pub const BABEL_LOGS_UDS_PATH: &str = "/var/lib/babel/logs.socket";
pub const JOBS_MONITOR_UDS_PATH: &str = "/var/lib/babel/jobs_monitor.socket";
const VSOCK_HOST_CID: u32 = 2;
const VSOCK_ENGINE_PORT: u32 = 40;

pub type BabelEngineClient = babel_api::babel::babel_engine_client::BabelEngineClient<
    InterceptedService<Channel, bv_utils::rpc::DefaultTimeout>,
>;

/// Trait that allows to inject custom babel_engine implementation.
pub trait BabelEngineConnector {
    fn connect(&self) -> BabelEngineClient;
}

pub struct VSockConnector;

impl BabelEngineConnector for VSockConnector {
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

/// Trait that allows to inject custom PAL implementation.
#[async_trait]
pub trait BabelPal {
    async fn mount_data_drive(&self, data_directory_mount_point: &str) -> Result<()>;
    async fn umount_data_drive(
        &self,
        data_directory_mount_point: &str,
        fuser_kill: bool,
    ) -> Result<()>;
    async fn is_data_drive_mounted(&self, data_directory_mount_point: &str) -> Result<bool>;
    async fn set_hostname(&self, hostname: &str) -> Result<()>;
    async fn set_swap_file(&self, swap_size_mb: u64, swap_file_location: &str) -> Result<()>;
    async fn is_swap_file_set(&self, swap_size_mb: u64, swap_file_location: &str) -> Result<bool>;
    async fn set_ram_disks(&self, ram_disks: Option<Vec<RamdiskConfiguration>>) -> Result<()>;
    async fn is_ram_disks_set(&self, ram_disks: Option<Vec<RamdiskConfiguration>>) -> Result<bool>;
}

pub async fn load_config(path: &Path) -> Result<BabelConfig> {
    info!("Loading babel configuration at {}", path.to_string_lossy());
    Ok(serde_json::from_str::<BabelConfig>(
        &fs::read_to_string(path).await?,
    )?)
}

pub async fn apply_babel_config<P: BabelPal>(pal: &P, config: &BabelConfig) -> Result<()> {
    pal.set_ram_disks(config.ramdisks.clone())
        .await
        .with_context(|| "failed to add ram disks")?;

    pal.mount_data_drive(babel_api::engine::DATA_DRIVE_MOUNT_POINT)
        .await
        .with_context(|| "failed to mount data disks")?;

    // swap could be put into data drive location
    // so we have to configure it after data drive mount
    pal.set_swap_file(config.swap_size_mb, &config.swap_file_location)
        .await
        .with_context(|| "failed to add swap file")?;

    Ok(())
}

pub async fn is_babel_config_applied<P: BabelPal>(pal: &P, config: &BabelConfig) -> Result<bool> {
    Ok(pal
        .is_swap_file_set(config.swap_size_mb, &config.swap_file_location)
        .await
        .with_context(|| "failed to check swap file")?
        && pal
            .is_ram_disks_set(config.ramdisks.clone())
            .await
            .with_context(|| "failed to add check disks")?
        && pal
            .is_data_drive_mounted(babel_api::engine::DATA_DRIVE_MOUNT_POINT)
            .await
            .with_context(|| "failed to check data disks")?)
}
