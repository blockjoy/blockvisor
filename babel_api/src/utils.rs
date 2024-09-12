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
