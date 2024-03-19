use crate::linux_platform::bv_root;
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use tokio::fs;
use tracing::info;

pub const CONFIG_PATH: &str = "var/lib/blockvisor/pal.json";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum PalConfig {
    LinuxFc,
    LinuxBare,
}

impl PalConfig {
    pub async fn load() -> Result<PalConfig> {
        let path = bv_root().join(CONFIG_PATH);
        if path.exists() {
            info!("Reading pal config: {}", path.display());
            let config = fs::read_to_string(&path)
                .await
                .with_context(|| format!("failed to read pal config: {}", path.display()))?;
            serde_json::from_str(&config)
                .with_context(|| format!("failed to parse pal config: {}", path.display()))
        } else {
            Ok(PalConfig::LinuxFc)
        }
    }
}
