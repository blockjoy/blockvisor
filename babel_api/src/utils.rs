use crate::engine::NodeEnv;
use eyre::Context;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

const PROTOCOL_DATA_LOCK_FILENAME: &str = ".protocol_data.lock";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BinaryStatus {
    Ok,
    ChecksumMismatch,
    Missing,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Binary {
    Destination(PathBuf),
    Bin(Vec<u8>),
    Checksum(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct BabelConfig {
    pub node_env: NodeEnv,
    /// RAM disks configuration.
    pub ramdisks: Vec<RamdiskConfiguration>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RamdiskConfiguration {
    /// Path to mount RAM disk to.
    pub ram_disk_mount_point: String,
    /// RAM disk size, in MB.
    pub ram_disk_size_mb: u64,
}

pub fn protocol_data_stamp(data_mount_point: &Path) -> eyre::Result<Option<SystemTime>> {
    let data_lock = data_mount_point.join(PROTOCOL_DATA_LOCK_FILENAME);
    if data_lock.exists() {
        Ok(Some(
            fs::File::open(data_lock)
                .and_then(|file| file.metadata().and_then(|meta| meta.modified()))
                .with_context(|| "failed to check .protocol_data.lock timestamp")?,
        ))
    } else {
        Ok(None)
    }
}

pub fn touch_protocol_data(data_mount_point: &Path) -> eyre::Result<()> {
    let data_lock = data_mount_point.join(PROTOCOL_DATA_LOCK_FILENAME);
    if !data_lock.exists() {
        fs::File::create(data_lock)?;
    } else {
        fs::File::open(data_lock)?.set_modified(SystemTime::now())?;
    }
    Ok(())
}
