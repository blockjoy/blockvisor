use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fmt::Debug, path::Path};
use tokio::fs;
use tracing::info;

pub const CONFIG_PATH: &str = "var/lib/blockvisor/pal.json";

#[macro_export]
macro_rules! load_pal {
    ($bv_root:expr) => {
        match $crate::pal_config::PalConfig::load($bv_root).await? {
            $crate::pal_config::PalConfig::LinuxFc => {
                $crate::linux_fc_platform::LinuxFcPlatform::new().await
            }
            $crate::pal_config::PalConfig::LinuxBare => {
                unimplemented!()
            }
        }
    };
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum PalConfig {
    LinuxFc,
    LinuxBare,
}

impl PalConfig {
    pub async fn load(bv_root: &Path) -> Result<PalConfig> {
        let path = bv_root.join(CONFIG_PATH);
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
