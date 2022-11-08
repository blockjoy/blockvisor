use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::info;
use uuid::Uuid;

use crate::node::FC_BIN_NAME;
use crate::utils::get_process_pid;
use crate::{network_interface::NetworkInterface, nodes::REGISTRY_CONFIG_DIR};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum NodeStatus {
    Running,
    Stopped,
    Failed,
}

impl fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeData {
    pub id: Uuid,
    pub name: String,
    pub image: String,
    pub expected_status: NodeStatus,
    pub network_interface: NetworkInterface,
    /// Whether or not this node should check for updates and then update itself. The default is
    /// `false`.
    #[serde(default)]
    pub self_update: bool,
}

impl NodeData {
    pub async fn load(path: &Path) -> Result<Self> {
        info!("Reading nodes config file: {}", path.display());
        fs::read_to_string(&path)
            .await
            .and_then(|s| toml::from_str::<Self>(&s).map_err(Into::into))
            .with_context(|| format!("Failed to read node file `{}`", path.display()))
    }

    pub async fn save(&self) -> Result<()> {
        let path = self.file_path();
        info!("Writing node config: {}", path.display());
        let config = toml::to_string(self)?;
        fs::write(&path, &*config).await?;

        Ok(())
    }

    pub async fn delete(self) -> Result<()> {
        let path = self.file_path();
        info!("Deleting node config: {}", path.display());
        fs::remove_file(&*path)
            .await
            .with_context(|| format!("Failed to delete node file `{}`", path.display()))?;

        self.network_interface.delete().await
    }

    fn file_path(&self) -> PathBuf {
        let filename = format!("{}.toml", self.id);
        REGISTRY_CONFIG_DIR.join(filename)
    }

    pub fn status(&self) -> NodeStatus {
        let cmd = self.id.to_string();
        let actual_status = match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(_) => NodeStatus::Running,
            Err(_) => NodeStatus::Stopped,
        };
        if actual_status == self.expected_status {
            actual_status
        } else {
            NodeStatus::Failed
        }
    }
}
