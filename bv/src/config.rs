use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs::{self, DirBuilder};
use tracing::info;

use crate::env::HOST_CONFIG_FILE;

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
}

impl Config {
    pub async fn load() -> Result<Config> {
        info!("Reading host config: {}", HOST_CONFIG_FILE.display());
        let config = fs::read_to_string(&*HOST_CONFIG_FILE).await?;
        Ok(toml::from_str(&config)?)
    }

    pub async fn save(&self) -> Result<()> {
        let parent = HOST_CONFIG_FILE.parent().unwrap();
        info!("Ensuring config dir is present: {}", parent.display());
        DirBuilder::new().recursive(true).create(parent).await?;
        info!("Writing host config: {}", HOST_CONFIG_FILE.display());
        let config = toml::Value::try_from(self)?;
        let config = toml::to_string(&config)?;
        fs::write(&*HOST_CONFIG_FILE, &*config).await?;
        Ok(())
    }

    pub async fn remove() -> Result<()> {
        info!("Removing host config: {}", HOST_CONFIG_FILE.display());
        fs::remove_file(&*HOST_CONFIG_FILE).await?;
        Ok(())
    }

    pub fn exists() -> bool {
        Path::new(&*HOST_CONFIG_FILE).exists()
    }
}
