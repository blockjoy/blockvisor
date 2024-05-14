use crate::{
    node_data::{self, NodeImage},
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

pub const REGISTRY_CONFIG_DIR: &str = "nodes";

pub fn build_registry_dir(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_DIR)
}

pub fn build_node_dir(bv_root: &Path, id: Uuid) -> PathBuf {
    bv_root
        .join(BV_VAR_PATH)
        .join(REGISTRY_CONFIG_DIR)
        .join(id.to_string())
}

#[derive(Debug)]
pub struct NodeContext {
    pub bv_root: PathBuf,
    pub vm_data_dir: PathBuf,
    pub plugin_data: PathBuf,
    pub plugin_script: PathBuf,
    pub node_config: PathBuf,
    pub registry: PathBuf,
}

impl NodeContext {
    pub fn build(pal: &impl Pal, id: Uuid) -> Self {
        let bv_root = pal.bv_root();
        let registry = build_registry_dir(bv_root);
        Self {
            bv_root: bv_root.to_path_buf(),
            vm_data_dir: pal.build_vm_data_path(id),
            plugin_data: registry.join(format!("{id}.data")),
            plugin_script: registry.join(format!("{id}.rhai")),
            node_config: node_data::file_path(id, &registry),
            registry,
        }
    }

    /// copy plugin script into nodes registry and read metadata form it
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
        if self.vm_data_dir.exists() {
            fs::remove_dir_all(&self.vm_data_dir)
                .await
                .with_context(|| {
                    format!(
                        "failed to delete node vm data `{}`",
                        self.vm_data_dir.display()
                    )
                })?;
        }
        remove_node_file(&self.node_config).await?;
        remove_node_file(&self.plugin_script).await?;
        remove_node_file(&self.plugin_data).await
    }
}

async fn remove_node_file(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .await
            .with_context(|| format!("failed to delete node file `{}`", path.display()))?;
    }
    Ok(())
}
