use eyre::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::debug;

const CONFIG_FILENAME: &str = ".nib.json";

pub fn default_blockvisor_port() -> u16 {
    9001
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    /// Client auth token.
    pub token: String,
    /// API endpoint url.
    pub blockjoy_api_url: String,
}

impl Config {
    pub async fn load() -> Result<Config> {
        let path = homedir::my_home()?
            .ok_or(anyhow!("can't get home directory"))?
            .join(CONFIG_FILENAME);
        if !path.exists() {
            bail!("Bib is not configured yet, please run `nib config` first.");
        }
        debug!("Reading nib config: {}", path.display());
        let config = fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read nib config: {}", path.display()))?;
        let config: Config = serde_json::from_str(&config)
            .with_context(|| format!("failed to parse nib config: {}", path.display()))?;
        Ok(config)
    }

    pub async fn save(&self) -> Result<()> {
        let path = homedir::my_home()?
            .ok_or(anyhow!("can't get home directory"))?
            .join(CONFIG_FILENAME);
        debug!("Writing nib config: {}", path.display());
        let config = serde_json::to_string(self)?;
        fs::write(&path, config)
            .await
            .with_context(|| format!("failed to save nib config: {}", path.display()))?;
        Ok(())
    }
}
