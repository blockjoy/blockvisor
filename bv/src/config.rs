use crate::services::api::{pb, AuthClient, AuthToken};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::{fs, sync::RwLockWriteGuard};
use tracing::{debug, info};

pub const CONFIG_PATH: &str = "etc/blockvisor.json";
pub const DEFAULT_BRIDGE_IFACE: &str = "bvbr0";

pub fn default_blockvisor_port() -> u16 {
    9001
}

pub fn default_iface() -> String {
    DEFAULT_BRIDGE_IFACE.to_string()
}

#[derive(Debug, Clone)]
pub struct SharedConfig {
    config: std::sync::Arc<tokio::sync::RwLock<Config>>,
    pub bv_root: std::path::PathBuf,
}

impl SharedConfig {
    pub fn new(config: Config, bv_root: std::path::PathBuf) -> Self {
        Self {
            config: std::sync::Arc::new(tokio::sync::RwLock::new(config)),
            bv_root,
        }
    }

    pub async fn read(&self) -> Config {
        self.config.read().await.clone()
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, Config> {
        self.config.write().await
    }

    pub async fn token(&self) -> Result<AuthToken> {
        let token = self.refreshed_token().await?;
        Ok(AuthToken(token))
    }

    async fn refreshed_token(&self) -> Result<String> {
        let token = &self.read().await.token;
        if AuthToken::expired(token)? {
            let mut write_lock = self.write().await;
            // A concurrent update may have written to the jwt field, check if the token has become
            // unexpired while we have unique access.
            if !AuthToken::expired(&write_lock.token)? {
                return Ok(write_lock.token.clone());
            }

            let req = pb::AuthServiceRefreshRequest {
                token: write_lock.token.clone(),
                refresh: Some(write_lock.refresh_token.clone()),
            };
            let mut service = AuthClient::connect(&write_lock.blockjoy_api_url).await?;
            let resp = service.refresh(req).await?;
            write_lock.token = resp.token.clone();
            write_lock.refresh_token = resp.refresh;
            write_lock.save(&self.bv_root).await?;
            Ok(resp.token)
        } else {
            Ok(token.clone())
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    /// Host uuid
    pub id: String,
    /// Host auth token
    pub token: String,
    /// The refresh token.
    pub refresh_token: String,
    /// API endpoint url
    pub blockjoy_api_url: String,
    /// Url for mqtt broker to receive commands and updates from.
    pub blockjoy_mqtt_url: Option<String>,
    /// Self update check interval - how often blockvisor shall check for new version of itself
    pub update_check_interval_secs: Option<u64>,
    /// Port to be used by blockvisor internal service
    #[serde(default = "default_blockvisor_port")]
    pub blockvisor_port: u16,
    /// Network interface name
    #[serde(default = "default_iface")]
    pub iface: String,
    /// Host's cluster id
    pub cluster_id: Option<String>,
    /// Addresses of the seed nodes for cluster discovery and announcements
    pub cluster_seed_urls: Option<Vec<String>>,
}

impl Config {
    pub async fn load(bv_root: &Path) -> Result<Config> {
        let path = bv_root.join(CONFIG_PATH);
        info!("Reading host config: {}", path.display());
        let config = fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read host config: {}", path.display()))?;
        let config = serde_json::from_str(&config)
            .with_context(|| format!("failed to parse host config: {}", path.display()))?;
        Ok(config)
    }

    pub async fn save(&self, bv_root: &Path) -> Result<()> {
        let path = bv_root.join(CONFIG_PATH);
        let parent = path.parent().unwrap();
        debug!("Ensuring config dir is present: {}", parent.display());
        fs::create_dir_all(parent).await?;
        info!("Writing host config: {}", path.display());
        let config = serde_json::to_string(&self)?;
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
