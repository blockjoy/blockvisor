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
use eyre::{Context, Result};
use std::path::Path;
use tokio::fs;
use tracing::info;

lazy_static::lazy_static! {
    pub static ref BABEL_LOGS_UDS_PATH: &'static Path = Path::new("/var/lib/babel/logs.socket");
    pub static ref JOBS_MONITOR_UDS_PATH: &'static Path = Path::new("/var/lib/babel/jobs_monitor.socket");
}

/// Trait that allows to inject custom PAL implementation.
#[async_trait]
pub trait BabelPal {
    async fn mount_data_drive(&self, data_directory_mount_point: &str) -> Result<()>;
    async fn umount_data_drive(&self, data_directory_mount_point: &str) -> Result<()>;
    async fn is_data_drive_mounted(&self, data_directory_mount_point: &str) -> Result<bool>;
    async fn set_hostname(&self, hostname: &str) -> Result<()>;
    async fn set_swap_file(&self, swap_size_mb: usize) -> Result<()>;
    async fn is_swap_file_set(&self, swap_size_mb: usize) -> Result<bool>;
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
    pal.set_swap_file(config.swap_size_mb)
        .await
        .with_context(|| "failed to add swap file")?;

    pal.set_ram_disks(config.ramdisks.clone())
        .await
        .with_context(|| "failed to add ram disks")?;

    pal.mount_data_drive(&config.data_directory_mount_point)
        .await
        .with_context(|| "failed to mount data disks")?;

    Ok(())
}

pub async fn is_babel_config_applied<P: BabelPal>(pal: &P, config: &BabelConfig) -> Result<bool> {
    Ok(pal
        .is_swap_file_set(config.swap_size_mb)
        .await
        .with_context(|| "failed to check swap file")?
        && pal
            .is_ram_disks_set(config.ramdisks.clone())
            .await
            .with_context(|| "failed to add check disks")?
        && pal
            .is_data_drive_mounted(&config.data_directory_mount_point)
            .await
            .with_context(|| "failed to check data disks")?)
}
