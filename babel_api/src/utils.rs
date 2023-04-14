use serde::{Deserialize, Serialize};

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
    Bin(Vec<u8>),
    Checksum(u32),
}
