pub mod async_pid_watch;
pub mod babel;
pub mod babel_service;
pub mod checksum;
pub mod chroot_platform;
pub mod compression;
pub mod download_job;
pub mod job_runner;
pub mod jobs;
pub mod jobs_manager;
pub mod log_buffer;
pub mod pal;
pub mod run_sh_job;
pub mod upload_job;
pub mod utils;

use babel_api::utils::BabelConfig;
use eyre::{Context, Result};
use std::path::Path;
use tokio::fs;
use tonic::{codegen::InterceptedService, transport::Channel};
use tracing::info;

lazy_static::lazy_static! {
    static ref NON_RETRIABLE: Vec<tonic::Code> = vec![tonic::Code::Internal,
        tonic::Code::InvalidArgument, tonic::Code::Unimplemented, tonic::Code::PermissionDenied];
}

#[macro_export]
macro_rules! with_selective_retry {
    ($fun:expr) => {{
        bv_utils::with_selective_retry!($fun, $crate::NON_RETRIABLE)
    }};
}

pub const PROTOCOL_DATA_LOCK_FILENAME: &str = ".protocol_data.lock";
pub const JOBS_MONITOR_UDS_PATH: &str = "/var/lib/babel/jobs_monitor.socket";
const POST_SETUP_SCRIPT: &str = "/var/lib/babel/post_setup.sh";

pub type BabelEngineClient = babel_api::babel::babel_engine_client::BabelEngineClient<
    InterceptedService<Channel, bv_utils::rpc::DefaultTimeout>,
>;

pub async fn load_config(path: &Path) -> Result<BabelConfig> {
    info!("Loading babel configuration at {}", path.to_string_lossy());
    Ok(serde_json::from_str::<BabelConfig>(
        &fs::read_to_string(path).await?,
    )?)
}

pub async fn apply_babel_config<P: pal::BabelPal>(pal: &P, config: &BabelConfig) -> Result<()> {
    pal.set_ram_disks(config.ramdisks.clone())
        .await
        .with_context(|| "failed to add ram disks")?;

    Ok(())
}

pub async fn is_babel_config_applied<P: pal::BabelPal>(
    pal: &P,
    config: &BabelConfig,
) -> Result<bool> {
    pal.is_ram_disks_set(config.ramdisks.clone())
        .await
        .with_context(|| "failed to add check disks")
}

pub fn is_protocol_data_locked(data_mount_point: &Path) -> bool {
    data_mount_point.join(PROTOCOL_DATA_LOCK_FILENAME).exists()
}
