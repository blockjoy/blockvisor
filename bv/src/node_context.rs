use crate::{
    node_data::{NodeData, NodeImage},
    pal::Pal,
    services::cookbook::{CookbookService, BABEL_PLUGIN_NAME, DATA_FILE},
    BV_VAR_PATH,
};
use babel_api::{metadata::BlockchainMetadata, rhai_plugin};
use bv_utils::cmd::run_cmd;
use eyre::{Context, Result};
use std::{
    ffi::OsStr,
    fmt::Debug,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::fs::{self};
use uuid::Uuid;

pub const REGISTRY_CONFIG_DIR: &str = "nodes";
pub const DATA_CACHE_DIR: &str = "blocks";
const DATA_CACHE_EXPIRATION: Duration = Duration::from_secs(7 * 24 * 3600);

pub fn build_registry_dir(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_DIR)
}

#[derive(Debug)]
pub struct NodeContext {
    pub bv_root: PathBuf,
    pub data_cache_dir: PathBuf,
    pub data_dir: PathBuf,
    pub plugin_data: PathBuf,
    pub plugin_script: PathBuf,
    pub registry: PathBuf,
}

impl NodeContext {
    pub fn build(pal: &impl Pal, id: Uuid) -> Self {
        let bv_root = pal.bv_root();
        let registry = build_registry_dir(bv_root);
        Self {
            bv_root: bv_root.to_path_buf(),
            data_cache_dir: bv_root.join(BV_VAR_PATH).join(DATA_CACHE_DIR),
            data_dir: pal.build_vm_data_path(id),
            plugin_data: registry.join(format!("{id}.data")),
            plugin_script: registry.join(format!("{id}.rhai")),
            registry,
        }
    }

    /// Create new data drive in chroot location, or copy it from cache
    pub async fn prepare_data_image<P: Pal>(&self, data: &NodeData<P::NetInterface>) -> Result<()> {
        fs::create_dir_all(&self.data_dir).await?;
        let path = self.data_dir.join(DATA_FILE);
        let data_cache_path = self
            .data_cache_dir
            .join(&data.image.protocol)
            .join(&data.image.node_type)
            .join(&data.network)
            .join(DATA_FILE);
        let disk_size_gb = data.requirements.disk_size_gb;

        // check local cache
        if data_cache_path.exists() {
            let elapsed = data_cache_path.metadata()?.created()?.elapsed()?;
            if elapsed < DATA_CACHE_EXPIRATION {
                run_cmd("cp", [data_cache_path.as_os_str(), path.as_os_str()]).await?;
                // TODO: ask Sean how to better resize images
                // in case cached image size is different from disk_size_gb
            } else {
                // clean up expired cache data
                fs::remove_file(data_cache_path).await?;
                // TODO: use cookbook to download new image
            }
        }

        // allocate new image on location, if it's not there yet
        if !path.exists() {
            let gb = &format!("{disk_size_gb}GB");
            run_cmd(
                "fallocate",
                [OsStr::new("-l"), OsStr::new(gb), path.as_os_str()],
            )
            .await?;
            run_cmd("mkfs.ext4", [path.as_os_str()]).await?;
        }

        Ok(())
    }

    /// copy plugin script into nodes registry and read metadata form it
    pub async fn copy_and_check_plugin(
        &self,
        image: &NodeImage,
    ) -> Result<(String, BlockchainMetadata)> {
        fs::copy(
            CookbookService::get_image_download_folder_path(&self.bv_root, image)
                .join(BABEL_PLUGIN_NAME),
            &self.plugin_script,
        )
        .await
        .with_context(|| format!("Babel plugin not found for {image}"))?;
        let script = fs::read_to_string(&self.plugin_script).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;
        Ok((script, metadata))
    }
}
