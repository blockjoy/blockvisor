use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs::{self, DirBuilder};
use tracing::info;

const HOST_CONFIG_FILENAME: &str = "blockvisor.toml";

lazy_static::lazy_static! {
    static ref HOST_CONFIG_FILE: PathBuf = home::home_dir()
        .map(|p| p.join(".config"))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(HOST_CONFIG_FILENAME);
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    pub id: String,
    pub token: String,
    pub blockjoy_api_url: String,
}

impl Config {
    pub async fn load() -> Result<Config> {
        info!("Reading host config: {}", HOST_CONFIG_FILE.display());
        let config = fs::read_to_string(&*HOST_CONFIG_FILE).await?;
        Ok(toml::from_str(&config)?)
    }

    pub async fn save(&self) -> Result<()> {
        let parent = &*HOST_CONFIG_FILE.parent().unwrap();
        if !Path::new(parent).exists() {
            info!("Ensuring config dir is present: {}", parent.display());
            DirBuilder::new().recursive(true).create(parent).await?
        }
        info!("Writing host config: {}", HOST_CONFIG_FILE.display());
        let config = toml::Value::try_from(self)?;
        let config = toml::to_string(&config)?;
        fs::write(&*HOST_CONFIG_FILE, &*config).await?;
        Ok(())
    }

    pub fn exists() -> bool {
        Path::new(&*HOST_CONFIG_FILE).exists()
    }
}
