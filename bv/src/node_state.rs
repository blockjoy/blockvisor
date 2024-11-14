use crate::bv_config::ApptainerConfig;
use crate::firewall;
use babel_api::utils::RamdiskConfiguration;
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
    pub store_id: String,
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

/// Node state data that we store in state file
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
    /// Size of data drive for storing protocol data (not to be confused with OS drive).
    pub disk_size_gb: u64,
    /// RAM disks configuration.
    pub ramdisks: Vec<RamdiskConfiguration>,
}

impl NodeState {
    pub async fn load(path: &Path) -> Result<Self> {
        info!("Reading node state file: {}", path.display());
        fs::read_to_string(&path)
            .await
            .and_then(|content| match serde_json::from_str::<Self>(&content) {
                Ok(state) => Ok(state),
                Err(err) => {
                    // LEGACY node support - remove once all nodes upgraded
                    if let Ok(legacy_state) = serde_json::from_str::<LegacyState>(&content) {
                        Ok(legacy_state.into())
                    } else {
                        error!("{err:#}");
                        Err(err.into())
                    }
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

// LEGACY node support - remove once all nodes upgraded
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct LegacyState {
    id: Uuid,
    name: String,
    expected_status: VmStatus,
    #[serde(default, with = "ts_seconds_option")]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    initialized: bool,
    image: LegacyImage,
    network_interface: NetInterface,
    #[serde(default)]
    assigned_cpus: Vec<usize>,
    requirements: Requirements,
    firewall_rules: Vec<firewall::Rule>,
    #[serde(default)]
    properties: NodeProperties,
    network: String,
    #[serde(default)]
    dev_mode: bool,
    #[serde(default)]
    restarting: bool,
    #[serde(default)]
    org_id: String,
    apptainer_config: Option<ApptainerConfig>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct NetInterface {
    ip: IpAddr,
    gateway: IpAddr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Requirements {
    vcpu_count: usize,
    mem_size_mb: u64,
    disk_size_gb: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct LegacyImage {
    protocol: String,
    node_type: String,
    node_version: String,
}

impl From<LegacyState> for NodeState {
    fn from(value: LegacyState) -> Self {
        Self {
            id: value.id,
            name: value.name.clone(),
            protocol_id: value.image.protocol.clone(),
            image_key: ProtocolImageKey {
                protocol_key: value.image.protocol.clone(),
                variant_key: value.image.node_type.clone(),
            },
            dev_mode: value.dev_mode,
            ip: value.network_interface.ip,
            gateway: value.network_interface.gateway,
            properties: value.properties,
            firewall: Default::default(), // no need to convert, since it won't be applied anyway
            display_name: value.name.clone(),
            org_id: value.org_id.clone(),
            org_name: value.org_id,
            protocol_name: value.image.protocol.clone(),
            dns_name: value.name,
            vm_config: VmConfig {
                vcpu_count: value.requirements.vcpu_count,
                mem_size_mb: value.requirements.mem_size_mb,
                disk_size_gb: value.requirements.disk_size_gb,
                ramdisks: vec![], // not used, so no need to convert
            },
            image: NodeImage {
                id: "00000000-0000-0000-0000-000000000000".to_string(),
                version: value.image.node_version.clone(),
                config_id: "00000000-0000-0000-0000-000000000000".to_string(),
                archive_id: "00000000-0000-0000-0000-000000000000".to_string(),
                store_id: format!(
                    "legacy://{}/{}/{}/{}",
                    value.image.protocol,
                    value.image.node_type,
                    value.image.node_version,
                    value.network
                ),
                uri: format!(
                    "legacy://{}/{}/{}",
                    value.image.protocol, value.image.node_type, value.image.node_version,
                ),
            },
            assigned_cpus: value.assigned_cpus,
            expected_status: value.expected_status,
            started_at: value.started_at,
            initialized: value.initialized,
            restarting: value.restarting,
            apptainer_config: value.apptainer_config,
        }
    }
}
