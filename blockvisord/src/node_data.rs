use anyhow::{Context, Result};
use cli_table::{
    format::Justify,
    CellStruct,
    Color::{Blue, Cyan, Green, Red, Yellow},
    Style, Table,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::info;
use zbus::zvariant::Type;

use crate::{network_interface::NetworkInterface, nodes::REGISTRY_CONFIG_DIR};

#[derive(Deserialize, Serialize, PartialEq, Eq, Clone, Copy, Debug, Type)]
pub enum NodeState {
    Running,
    Stopped,
}

fn style_node_state(cell: CellStruct, value: &NodeState) -> CellStruct {
    match value {
        NodeState::Running => cell.foreground_color(Some(Green)),
        NodeState::Stopped => cell.foreground_color(Some(Red)),
    }
}

impl fmt::Display for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Type, Table)]
pub struct NodeData {
    #[table(title = "VM ID", justify = "Justify::Right", color = "Cyan")]
    pub id: String,
    #[table(title = "Chain", color = "Blue")]
    pub chain: String,
    #[table(title = "State", customize_fn = "style_node_state")]
    pub state: NodeState,
    #[table(title = "IP Address", color = "Yellow")]
    pub network_interface: NetworkInterface,
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
            .with_context(|| format!("Failed to delete node file `{}`", path.display()))
    }

    fn file_path(&self) -> PathBuf {
        let filename = format!("{}.toml", self.id);
        REGISTRY_CONFIG_DIR.join(filename)
    }
}
