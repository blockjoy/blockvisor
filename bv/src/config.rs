use crate::services::api::{pb, AuthClient, AuthToken};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::{
    fs::{self, DirBuilder},
    sync::RwLockWriteGuard,
};
use tracing::info;

pub const CONFIG_PATH: &str = "etc/blockvisor.json";

pub fn default_blockvisor_port() -> u16 {
    9001
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
        let read_lock = self.read().await;
        if AuthToken::expired(&read_lock.token)? {
            // Explicity drop the lock. Note that if we do not drop the lock here, the succeeding
            // call to `self.write()` will immediately deadlock.
            drop(read_lock);
            let mut write_lock = self.write().await;
            // A concurrent update may have written to the jwt field, check if the token has become
            // unexpired while we have unique access.
            if AuthToken::expired(&write_lock.token)? {
                return Ok(write_lock.token.clone());
            }

            let req = pb::AuthServiceRefreshRequest {
                token: write_lock.token.clone(),
                refresh: Some(write_lock.refresh_token.clone()),
            };
            let mut service = AuthClient::connect(self).await?;
            let resp = service.refresh(req).await?;
            write_lock.token = resp.token.clone();
            write_lock.refresh_token = resp.refresh;
            write_lock.save(&self.bv_root).await?;
            Ok(resp.token)
        } else {
            Ok(read_lock.token)
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
    /// The token used for talking to cookbook.
    pub cookbook_token: String,
    /// API endpoint url
    pub blockjoy_api_url: String,
    /// Url of key service for getting secrets
    pub blockjoy_keys_url: Option<String>,
    /// Url of cookbook service for getting fs images, babel configs, kernel files, etc.
    pub blockjoy_registry_url: Option<String>,
    /// Url for mqtt broker to receive commands and updates from.
    pub blockjoy_mqtt_url: Option<String>,
    /// Self update check interval - how often blockvisor shall check for new version of itself
    pub update_check_interval_secs: Option<u64>,
    /// Port to be used by blockvisor internal service
    #[serde(default = "default_blockvisor_port")]
    pub blockvisor_port: u16,
}

impl Config {
    pub async fn load(bv_root: &Path) -> Result<Config> {
        let path = bv_root.join(CONFIG_PATH);
        info!("Reading host config: {}", path.display());
        let config = fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&config)?)
    }

    pub async fn save(&self, bv_root: &Path) -> Result<()> {
        let path = bv_root.join(CONFIG_PATH);
        let parent = path.parent().unwrap();
        info!("Ensuring config dir is present: {}", parent.display());
        DirBuilder::new().recursive(true).create(parent).await?;
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
