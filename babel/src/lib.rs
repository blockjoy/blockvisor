pub mod async_pid_watch;
pub mod babel;
pub mod babel_service;
pub mod babelsup_service;
pub mod checksum;
pub mod chroot_platform;
pub mod compression;
pub mod download_job;
pub mod fc_platform;
pub mod job_runner;
pub mod jobs;
pub mod jobs_manager;
pub mod log_buffer;
pub mod logs_service;
pub mod pal;
pub mod pal_config;
pub mod run_sh_job;
pub mod supervisor;
pub mod ufw_wrapper;
pub mod upload_job;
pub mod utils;

use babel_api::metadata::BabelConfig;
use eyre::{Context, Result};
use std::path::Path;
use tokio::fs;
use tonic::{codegen::InterceptedService, transport::Channel};
use tracing::info;

pub const BABEL_LOGS_UDS_PATH: &str = "/var/lib/babel/logs.socket";
pub const JOBS_MONITOR_UDS_PATH: &str = "/var/lib/babel/jobs_monitor.socket";
const NODE_ENV_FILE_PATH: &str = "/var/lib/babel/node_env";
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

pub async fn is_babel_config_applied<P: pal::BabelPal>(
    pal: &P,
    config: &BabelConfig,
) -> Result<bool> {
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
