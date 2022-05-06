use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct APIClient {
    inner: reqwest::blocking::Client,
    base_url: reqwest::Url,
}

impl APIClient {
    pub fn new(base_url: String, timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()?;
        Ok(Self {
            inner: client,
            base_url: base_url.parse()?,
        })
    }

    pub fn register_host(&self, otp: &str, info: &HostInfo) -> Result<HostCredentials> {
        let url = format!("{}/hosts", self.base_url.as_str().trim_end_matches('/'));
        let body = serde_json::to_string(info)?;

        let text = self
            .inner
            .post(url)
            .header("Content-Type", "application/json")
            .bearer_auth(otp)
            .body(body)
            .send()?
            .text()?;
        let creds: HostCredentials = serde_json::from_str(&text)?;

        Ok(creds)
    }

    pub fn get_pending_commands(&self, token: &str, host_id: &str) -> Result<Vec<Command>> {
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
            .send()?
            .text()?;
        let commands: Vec<Command> = serde_json::from_str(&text)?;

        Ok(commands)
    }

    pub fn update_command_status(
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
        let body = serde_json::to_string(update)?;

        let text = self
            .inner
            .put(url)
            .header("Content-Type", "application/json")
            .bearer_auth(token)
            .body(body)
            .send()?
            .text()?;
        let command: Command = serde_json::from_str(&text)?;

        Ok(command)
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct HostInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub ip_addr: String,
    pub os_flavor: String,
    pub os_kernel_version: String,
    pub cpu_count: usize,
    pub mem_size: usize,
    pub hostname: String,
    pub disk_size: usize,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct HostCredentials {
    pub host_id: String,
    pub token: String,
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
pub struct CommandStatusUpdate {
    pub response: String,
    pub exit_status: i32,
}
