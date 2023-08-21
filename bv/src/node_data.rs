use anyhow::{Context, Result};
use babel_api::metadata::{firewall, Requirements};
use chrono::serde::ts_seconds_option;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::info;
use uuid::Uuid;

use crate::pal::NetInterface;

pub type NodeProperties = HashMap<String, String>;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum NodeStatus {
    Running,
    Stopped,
    Busy,
    Failed,
}

impl fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct NodeImage {
    pub protocol: String,
    pub node_type: String,
    pub node_version: String,
}

impl fmt::Display for NodeImage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}",
            self.protocol, self.node_type, self.node_version
        )
    }
}

// Data that we store in data file
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeData<N> {
    pub id: Uuid,
    pub name: String,
    pub expected_status: NodeStatus,
    #[serde(default, with = "ts_seconds_option")]
    /// Time when node was started, None if node should not be running now
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub initialized: bool,
    pub image: NodeImage,
    pub kernel: String,
    pub network_interface: N,
    pub requirements: Requirements,
    pub firewall_rules: Vec<firewall::Rule>,
    #[serde(default)]
    pub properties: NodeProperties,
    pub network: String,
}

// Data that we display in cli
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeDisplayInfo {
    pub id: Uuid,
    pub name: String,
    pub status: NodeStatus,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub uptime: Option<i64>,
}

impl<N: NetInterface + Serialize + DeserializeOwned> NodeData<N> {
    pub async fn load(path: &Path) -> Result<Self> {
        info!("Reading nodes config file: {}", path.display());
        fs::read_to_string(&path)
            .await
            .and_then(|s| serde_json::from_str::<Self>(&s).map_err(Into::into))
            .with_context(|| format!("Failed to read node file `{}`", path.display()))
    }

    pub async fn save(&self, registry_config_dir: &Path) -> Result<()> {
        let path = self.file_path(registry_config_dir);
        info!("Writing node config: {}", path.display());
        let config = serde_json::to_string(self)?;
        fs::write(&path, &*config).await?;

        Ok(())
    }

    pub async fn delete(self, registry_config_dir: &Path) -> Result<()> {
        let path = self.file_path(registry_config_dir);
        info!("Deleting node config: {}", path.display());
        fs::remove_file(&path)
            .await
            .with_context(|| format!("Failed to delete node file `{}`", path.display()))?;

        self.network_interface.delete().await
    }

    fn file_path(&self, registry_config_dir: &Path) -> PathBuf {
        let filename = format!("{}.json", self.id);
        registry_config_dir.join(filename)
    }
}
