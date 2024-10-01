use crate::api_config::ApiConfig;
use crate::services::AuthToken;
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use sysinfo::{System, SystemExt};
use tokio::fs;
use tracing::debug;

pub const CONFIG_PATH: &str = "etc/blockvisor.json";
pub const DEFAULT_BRIDGE_IFACE: &str = "bvbr0";

pub fn default_blockvisor_port() -> u16 {
    9001
}

pub fn default_iface() -> String {
    DEFAULT_BRIDGE_IFACE.to_string()
}

pub fn default_hostname() -> String {
    let mut sys = System::new_all();
    sys.refresh_all();
    sys.host_name().unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct SharedConfig {
    pub config: std::sync::Arc<tokio::sync::RwLock<Config>>,
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

    pub async fn set_mqtt_url(&self, mqtt_url: Option<String>) {
        self.config.write().await.blockjoy_mqtt_url = mqtt_url;
    }

    pub async fn token(&self) -> Result<AuthToken, tonic::Status> {
        let api_config = self.read().await.api_config.clone();
        Ok(AuthToken(if api_config.token_expired()? {
            let token = {
                let mut write_lock = self.config.write().await;
                // A concurrent update may have written to the jwt field, check if the token has become
                // unexpired while we have unique access.
                if write_lock.api_config.token_expired()? {
                    write_lock.api_config.refresh_token().await?;
                }
                write_lock.api_config.token.clone()
            };
            self.read()
                .await
                .save(&self.bv_root)
                .await
                .map_err(|err| tonic::Status::internal(format!("failed to save token: {err:#}")))?;
            token
        } else {
            api_config.token
        }))
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct ApptainerConfig {
    pub extra_args: Option<Vec<String>>,
    pub host_network: bool,
    pub cpu_limit: bool,
    pub memory_limit: bool,
}

impl Default for ApptainerConfig {
    fn default() -> Self {
        Self {
            extra_args: None,
            host_network: false,
            cpu_limit: true,
            memory_limit: true,
        }
    }
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    /// Host uuid
    pub id: String,
    /// Host name
    #[serde(default = "default_hostname")]
    pub name: String,
    #[serde(flatten)]
    pub api_config: ApiConfig,
    /// Url for mqtt broker to receive commands and updates from.
    pub blockjoy_mqtt_url: Option<String>,
    /// Self update check interval - how often blockvisor shall check for new version of itself
    pub update_check_interval_secs: Option<u64>,
    /// Port to be used by blockvisor internal service
    /// 0 has special meaning - pick first free port
    #[serde(default = "default_blockvisor_port")]
    pub blockvisor_port: u16,
    /// Network interface name
    #[serde(default = "default_iface")]
    pub iface: String,
    /// Host's cluster id
    pub cluster_id: Option<String>,
    /// Cluster gossip listen port
    pub cluster_port: Option<u32>,
    /// Addresses of the seed nodes for cluster discovery and announcements
    pub cluster_seed_urls: Option<Vec<String>>,
    /// Apptainer configuration
    pub apptainer: ApptainerConfig,
}

impl Config {
    pub async fn load(bv_root: &Path) -> Result<Config> {
        let path = bv_root.join(CONFIG_PATH);
        debug!("Reading host config: {}", path.display());
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
        debug!("Writing host config: {}", path.display());
        let config = serde_json::to_string(&self)?;
        fs::write(path, config).await?;
        Ok(())
    }
}
