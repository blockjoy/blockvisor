use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::path::PathBuf;
use tokio::fs;
use tracing::info;

pub const CONFIG_FILENAME: &str = "pal.json";
pub const BABEL_VAR_DIR: &str = "/var/lib/babel";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum PalConfig {
    Fc,
    Chroot,
}

pub async fn load() -> Result<PalConfig> {
    let path = PathBuf::from(BABEL_VAR_DIR).join(CONFIG_FILENAME);
    if path.exists() {
        info!("Reading pal config: {}", path.display());
        let config = fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read pal config: {}", path.display()))?;
        serde_json::from_str(&config)
            .with_context(|| format!("failed to parse pal config: {}", path.display()))
    } else {
        Ok(PalConfig::Fc)
    }
}

pub async fn save(config: PalConfig) -> Result<()> {
    fs::create_dir_all(BABEL_VAR_DIR).await?;
    let path = PathBuf::from(BABEL_VAR_DIR).join(CONFIG_FILENAME);
    info!("Saving pal config: {}", path.display());
    fs::write(
        &path,
        serde_json::to_string(&config)
            .with_context(|| format!("failed to serialize pal config: {}", path.display()))?,
    )
    .await
    .with_context(|| format!("failed to write pal config: {}", path.display()))
}
