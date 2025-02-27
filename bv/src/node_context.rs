use crate::BV_VAR_PATH;
use eyre::{Context, Result};
use std::{
    fmt::Debug,
    path::{Path, PathBuf},
};
use tokio::fs::{self};
use uuid::Uuid;

pub const NODES_DIR: &str = "nodes";

pub fn build_nodes_dir(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(NODES_DIR)
}

pub fn build_node_dir(bv_root: &Path, id: Uuid) -> PathBuf {
    build_nodes_dir(bv_root).join(id.to_string())
}

#[derive(Debug, Clone)]
pub struct NodeContext {
    pub plugin_data: PathBuf,
    pub plugin_config: PathBuf,
    pub nodes_dir: PathBuf,
    pub node_dir: PathBuf,
}

impl NodeContext {
    pub fn build(bv_root: &Path, id: Uuid) -> Self {
        let node_dir = build_node_dir(bv_root, id);
        let nodes_dir = build_nodes_dir(bv_root);
        Self {
            plugin_data: node_dir.join("plugin.data"),
            plugin_config: node_dir.join("plugin_config.json"),
            nodes_dir,
            node_dir,
        }
    }

    pub async fn delete(&self) -> Result<()> {
        if self.node_dir.exists() {
            fs::remove_dir_all(&self.node_dir).await.with_context(|| {
                format!("failed to delete node dir `{}`", self.node_dir.display())
            })?;
        }
        Ok(())
    }
}
