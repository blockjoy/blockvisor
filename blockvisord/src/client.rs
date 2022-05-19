use anyhow::Result;
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
