use crate::{
    node_state::NodeImage,
    pal::Pal,
    services::blockchain::{self, BABEL_PLUGIN_NAME},
    BV_VAR_PATH,
};
use babel_api::{metadata::BlockchainMetadata, rhai_plugin};
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

#[derive(Debug)]
pub struct NodeContext {
    pub bv_root: PathBuf,
    pub plugin_data: PathBuf,
    pub plugin_script: PathBuf,
    pub nodes_dir: PathBuf,
    pub node_dir: PathBuf,
}

impl NodeContext {
    pub fn build(pal: &impl Pal, id: Uuid) -> Self {
        let bv_root = pal.bv_root();
        let node_dir = build_node_dir(bv_root, id);
        let nodes_dir = build_nodes_dir(bv_root);
        Self {
            bv_root: bv_root.to_path_buf(),
            plugin_data: node_dir.join("plugin.data"),
            plugin_script: node_dir.join("babel.rhai"),
            nodes_dir,
            node_dir,
        }
    }

    /// copy plugin script into nodes state and read metadata form it
    pub async fn copy_and_check_plugin(
        &self,
        image: &NodeImage,
    ) -> Result<(String, BlockchainMetadata)> {
        fs::copy(
            blockchain::get_image_download_folder_path(&self.bv_root, image)
                .join(BABEL_PLUGIN_NAME),
            &self.plugin_script,
        )
        .await
        .with_context(|| format!("Babel plugin not found for {image}"))?;
        let script = fs::read_to_string(&self.plugin_script).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;
        Ok((script, metadata))
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
