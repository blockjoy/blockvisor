use anyhow::Result;
use std::time::Duration;

use crate::types::{Command, CommandStatusUpdate, HostCreateRequest, HostCredentials};

pub struct APIClient {
    inner: reqwest::Client,
    base_url: reqwest::Url,
}

impl APIClient {
    pub fn new(base_url: String, timeout: Duration) -> Result<Self> {
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
    ) -> Result<HostCredentials> {
        let url = format!("{}/hosts", self.base_url.as_str().trim_end_matches('/'));
        let body = serde_json::to_string(create)?;

        let text = self
            .inner
            .post(url)
            .header("Content-Type", "application/json")
            .bearer_auth(otp)
            .body(body)
            .send()
            .await?
            .text()
            .await?;
        let creds: HostCredentials = serde_json::from_str(&text)?;

        Ok(creds)
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
        let body = serde_json::to_string(update)?;

        let text = self
            .inner
            .put(url)
            .header("Content-Type", "application/json")
            .bearer_auth(token)
            .body(body)
            .send()
            .await?
            .text()
            .await?;
        let command: Command = serde_json::from_str(&text)?;

        Ok(command)
    }
}
