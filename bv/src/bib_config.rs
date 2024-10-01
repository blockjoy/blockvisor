use crate::bv_config;
use eyre::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs;
use tracing::{debug, warn};

const CONFIG_FILENAME: &str = ".bib.json";

pub fn default_blockvisor_port() -> u16 {
    9001
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    /// Client auth token.
    pub token: String,
    /// API endpoint url.
    pub blockjoy_api_url: String,
    /// Port used by blockvisor internal service.
    #[serde(skip)]
    pub blockvisor_port: Option<u16>,
}

impl Config {
    pub async fn load(bv_root: &Path) -> Result<Config> {
        let path = homedir::my_home()?
            .ok_or(anyhow!("can't get home directory"))?
            .join(CONFIG_FILENAME);
        if !path.exists() {
            bail!("Bib is not configured yet, please run `bib config` first.");
        }
        debug!("Reading bib config: {}", path.display());
        let config = fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read bib config: {}", path.display()))?;
        let mut config: Config = serde_json::from_str(&config)
            .with_context(|| format!("failed to parse bib config: {}", path.display()))?;

        if let Some(bv_config) = Self::load_bv_config(bv_root).await {
            config.blockvisor_port = Some(bv_config.blockvisor_port)
        }
        Ok(config)
    }

    pub async fn load_bv_config(bv_root: &Path) -> Option<bv_config::Config> {
        let bv_path = bv_root.join(bv_config::CONFIG_PATH);
        if bv_path.exists() {
            if let Ok(bv_config) = fs::read_to_string(&bv_path).await {
                if let Ok(bv_config) = serde_json::from_str::<bv_config::Config>(&bv_config) {
                    return Some(bv_config);
                } else {
                    warn!("failed to parse bv config: {}", bv_path.display());
                }
            } else {
                warn!("failed to read bv config: {}", bv_path.display());
            }
        }
        None
    }

    pub async fn save(&self) -> Result<()> {
        let path = homedir::my_home()?
            .ok_or(anyhow!("can't get home directory"))?
            .join(CONFIG_FILENAME);
        debug!("Writing bib config: {}", path.display());
        let config = serde_json::to_string(self)?;
        fs::write(&path, config)
            .await
            .with_context(|| format!("failed to save bib config: {}", path.display()))?;
        Ok(())
    }
}
