use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs::{self, DirBuilder};
use tracing::info;

pub const CONFIG_PATH: &str = "etc/blockvisor.toml";

pub fn default_blockvisor_port() -> u32 {
    9001
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    /// Host uuid
    pub id: String,
    /// Host auth token
    pub token: String,
    /// API endpoint url
    pub blockjoy_api_url: String,
    /// Url of key service for getting secrets
    pub blockjoy_keys_url: String,
    /// Url of cookbook service for getting fs images, babel configs, kernel files, etc.
    pub blockjoy_registry_url: String,
    /// Self update check interval - how often blockvisor shall check for new version of itself
    pub update_check_interval_secs: Option<u64>,
    /// Port to be used by blockvisor internal service
    #[serde(default = "default_blockvisor_port")]
    pub blockvisor_port: u32,
}

impl Config {
    pub async fn load(bv_root: &Path) -> Result<Config> {
        let path = bv_root.join(CONFIG_PATH);
        info!("Reading host config: {}", path.display());
        let config = fs::read_to_string(path).await?;
        Ok(toml::from_str(&config)?)
    }

    pub async fn save(&self, bv_root: &Path) -> Result<()> {
        let path = bv_root.join(CONFIG_PATH);
        let parent = path.parent().unwrap();
        info!("Ensuring config dir is present: {}", parent.display());
        DirBuilder::new().recursive(true).create(parent).await?;
        info!("Writing host config: {}", path.display());
        let config = toml::Value::try_from(self)?;
        let config = toml::to_string(&config)?;
        fs::write(path, &*config).await?;
        Ok(())
    }

    pub async fn remove(bv_root: &Path) -> Result<()> {
        let path = bv_root.join(CONFIG_PATH);
        info!("Removing host config: {}", path.display());
        fs::remove_file(path).await?;
        Ok(())
    }
}
