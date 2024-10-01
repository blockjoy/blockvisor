use crate::bv_config::ApptainerConfig;
use crate::firewall;
use babel_api::utils::BabelConfig;
use chrono::serde::ts_seconds_option;
use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fmt::Debug;
use std::net::IpAddr;
use std::path::Path;
use tokio::fs;
use tracing::{error, info};
use uuid::Uuid;

pub const NODE_STATE_FILENAME: &str = "state.json";
pub type NodeProperties = HashMap<String, String>;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum VmStatus {
    Running,
    Stopped,
    Busy,
    Failed,
}

impl fmt::Display for VmStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct NodeImage {
    pub id: String,
    pub version: String,
    pub config_id: String,
    pub archive_id: String,
    pub uri: String,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct NodeInfo {
    pub id: Uuid,
    pub name: String,
    pub status: VmStatus,

    pub uptime: Option<i64>,
    pub requirements: Option<VmConfig>,
    pub properties: NodeProperties,
    pub assigned_cpus: Vec<usize>,
    pub dev_mode: bool,
}

// NodeData that we store in state file
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct NodeState {
    // static properties
    pub id: Uuid,
    pub name: String,
    pub protocol_id: String,
    pub image_key: ProtocolImageKey,
    pub dev_mode: bool,
    // potentially configurable
    pub ip: IpAddr,
    pub gateway: IpAddr,

    // dynamic
    pub properties: NodeProperties,
    pub firewall: firewall::Config,

    // dynamic-description
    pub display_name: String,
    pub org_id: String,
    pub org_name: String,
    pub protocol_name: String,
    pub dns_name: String,

    // upgradeable
    pub vm_config: VmConfig,
    pub image: NodeImage,

    // internal state
    pub assigned_cpus: Vec<usize>,
    pub expected_status: VmStatus,
    #[serde(default, with = "ts_seconds_option")]
    /// Time when node was started, None if node should not be running now
    pub started_at: Option<DateTime<Utc>>,
    pub initialized: bool,
    pub restarting: bool,
    pub apptainer_config: Option<ApptainerConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProtocolImageKey {
    /// The key identifier to a protocol.
    pub protocol_key: String,
    /// A unique identifier to the implementation variant (e.g. reth or geth).
    pub variant_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct VmConfig {
    /// Virtual cores to share with VM.
    pub vcpu_count: usize,
    /// RAM allocated to VM in MB.
    pub mem_size_mb: u64,
    /// Size of data drive for storing blockchain data (not to be confused with OS drive).
    pub disk_size_gb: u64,
    pub babel_config: BabelConfig,
}

impl NodeState {
    pub async fn load(path: &Path) -> Result<Self> {
        info!("Reading node state file: {}", path.display());
        fs::read_to_string(&path)
            .await
            .and_then(|s| match serde_json::from_str::<Self>(&s) {
                Ok(r) => Ok(r),
                Err(err) => {
                    error!("{err:#}");
                    Err(err.into())
                }
            })
            .with_context(|| format!("Failed to read node state file `{}`", path.display()))
    }

    pub async fn save(&self, nodes_dir: &Path) -> Result<()> {
        let path = nodes_dir
            .join(self.id.to_string())
            .join(NODE_STATE_FILENAME);
        info!("Writing node state: {}", path.display());
        let config = serde_json::to_string(self)?;
        fs::write(&path, &*config).await?;
        Ok(())
    }
}
