use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

pub struct APIClient {
    inner: reqwest::Client,
    base_url: reqwest::Url,
}

impl APIClient {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            inner: client,
            base_url: base_url.parse()?,
        })
    }

    pub async fn register_host(
        &self,
        otp: &str,
        create: &HostCreateRequest,
    ) -> Result<HostCreateResponse> {
        let url = format!(
            "{}/host_provisions/{}/hosts",
            self.base_url.as_str().trim_end_matches('/'),
            otp
        );

        let text = self
            .inner
            .post(url)
            .header("Content-Type", "application/json")
            .json(create)
            .send()
            .await?
            .text()
            .await?;
        let host: HostCreateResponse = serde_json::from_str(&text)?;

        Ok(host)
    }

    pub async fn delete_host(&self, token: &str, host_id: &str) -> Result<()> {
        let url = format!(
            "{}/hosts/{}",
            self.base_url.as_str().trim_end_matches('/'),
            host_id
        );

        let resp = self
            .inner
            .delete(url)
            .header("Content-Type", "application/json")
            .bearer_auth(token)
            .send()
            .await?;

        if !resp.status().is_success() {
            let error = resp.text().await?;
            bail!("Cannot delete host `{host_id}`: {error}");
        }

        Ok(())
    }

    pub async fn get_pending_commands(&self, token: &str, host_id: &str) -> Result<Vec<Command>> {
        let url = format!(
            "{}/hosts/{}/commands/pending",
            self.base_url.as_str().trim_end_matches('/'),
            host_id
        );

        let text = self
            .inner
            .get(url)
            .header("Content-Type", "application/json")
            .bearer_auth(token)
            .send()
            .await?
            .text()
            .await?;
        let commands: Vec<Command> = serde_json::from_str(&text)?;

        Ok(commands)
    }

    pub async fn update_command_status(
        &self,
        token: &str,
        command_id: &str,
        update: &CommandStatusUpdate,
    ) -> Result<Command> {
        let url = format!(
            "{}/commands/{}/response",
            self.base_url.as_str().trim_end_matches('/'),
            command_id
        );

        let text = self
            .inner
            .put(url)
            .header("Content-Type", "application/json")
            .bearer_auth(token)
            .json(update)
            .send()
            .await?
            .text()
            .await?;
        let command: Command = serde_json::from_str(&text)?;

        Ok(command)
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct HostCreateRequest {
    pub org_id: Option<Uuid>,
    pub name: String,
    pub version: Option<String>,
    pub location: Option<String>,
    pub cpu_count: Option<i64>,
    pub mem_size: Option<i64>,
    pub disk_size: Option<i64>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub ip_addr: String,
    pub val_ip_addrs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCreateResponse {
    pub id: Uuid,
    pub org_id: Option<Uuid>,
    pub name: String,
    pub version: Option<String>,
    pub cpu_count: Option<i64>,
    pub mem_size: Option<i64>,
    pub disk_size: Option<i64>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub location: Option<String>,
    pub ip_addr: String,
    pub val_ip_addrs: Option<String>,
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub nodes: Option<Vec<Node>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: Uuid,
    pub org_id: Uuid,
    pub host_id: Uuid,
    pub name: Option<String>,
    pub groups: Option<String>,
    pub version: Option<String>,
    pub ip_addr: Option<String>,
    pub blockchain_id: Uuid,
    pub node_type: NodeType,
    pub address: Option<String>,
    pub wallet_address: Option<String>,
    pub block_height: Option<i64>,
    pub node_data: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: NodeStatus,
    pub is_online: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Available,
    Broadcasting,
    Cancelled,
    Consensus,
    Creating,
    Delegating,
    Delinquent,
    Disabled,
    Earning,
    Electing,
    Elected,
    Exporting,
    Ingesting,
    Installing,
    Migrating,
    Mining,
    Minting,
    Processing,
    Relaying,
    Removed,
    Removing,
    Running,
    Snapshoting,
    Staked,
    Staking,
    Started,
    Starting,
    Stopped,
    Stopping,
    Synced,
    Syncing,
    Upgrading,
    Validating,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Api,
    Etl,
    Node,
    Oracle,
    Relay,
    Validator,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Command {
    pub id: String,
    pub host_id: String,
    pub cmd: String,
    pub sub_cmd: Option<String>,
    pub response: Option<String>,
    pub exit_status: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CommandCreateRequest {
    pub cmd: String,
    pub sub_cmd: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CommandStatusUpdate {
    pub response: String,
    pub exit_status: i32,
}
