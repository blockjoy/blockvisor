use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
