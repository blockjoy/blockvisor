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
pub const DEFAULT_SERVICES_PATH: &str = "var/lib/babel/services.rhai";
pub const BABEL_VAR_PATH: &str = "var/lib/babel";

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
            plugin_script: node_dir.join(BABEL_PLUGIN_NAME),
            nodes_dir,
            node_dir,
        }
    }

    /// copy plugin script into nodes state and read metadata form it
    pub async fn copy_and_check_plugin(
        &self,
        image: &NodeImage,
        rootfs_dir: &Path,
    ) -> Result<(String, BlockchainMetadata)> {
        fs::copy(
            blockchain::get_image_download_folder_path(&self.bv_root, image)
                .join(BABEL_PLUGIN_NAME),
            &self.plugin_script,
        )
        .await
        .with_context(|| format!("Babel plugin not found for {image}"))?;
        self.load_script(rootfs_dir).await
    }

    pub async fn load_script(&self, rootfs_dir: &Path) -> Result<(String, BlockchainMetadata)> {
        let mut script = fs::read_to_string(&self.plugin_script).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;
        let services_path = rootfs_dir.join(DEFAULT_SERVICES_PATH);
        if services_path.exists() {
            script.push_str(&fs::read_to_string(services_path).await?);
        }

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
