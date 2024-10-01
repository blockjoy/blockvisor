use crate::services::{request_refresh_token, AuthToken};
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::timeout;

const REFRESH_TOKEN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct ApiConfig {
    /// Client auth token
    pub token: String,
    /// The refresh token.
    pub refresh_token: String,
    /// API endpoint url
    pub blockjoy_api_url: String,
}

impl ApiConfig {
    pub fn token_expired(&self) -> Result<bool, tonic::Status> {
        AuthToken::expired(&self.token)
    }

    pub async fn refresh_token(&mut self) -> Result<(), tonic::Status> {
        let resp = timeout(
            REFRESH_TOKEN_TIMEOUT,
            request_refresh_token(&self.blockjoy_api_url, &self.token, &self.refresh_token),
        )
        .await
        .map_err(|_| {
            tonic::Status::deadline_exceeded(format!(
                "refresh token request has stacked for more than {}s",
                REFRESH_TOKEN_TIMEOUT.as_secs()
            ))
        })??;
        self.token = resp.token;
        self.refresh_token = resp.refresh;
        Ok(())
    }
}
