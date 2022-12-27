use anyhow::{Context, Result};
use tonic::transport::Channel;
use uuid::Uuid;

use crate::services::grpc::{pb, with_auth};

pub struct KeyService {
    token: String,
    client: pb::key_files_client::KeyFilesClient<Channel>,
}

impl KeyService {
    pub async fn connect(url: &str, token: &str) -> Result<Self> {
        let client = pb::key_files_client::KeyFilesClient::connect(url.to_string())
            .await
            .context("Failed to connect to key service")?;
        Ok(Self {
            token: token.to_string(),
            client,
        })
    }

    pub async fn download_keys(&mut self, node_id: &Uuid) -> Result<Vec<pb::Keyfile>> {
        let req = pb::KeyFilesGetRequest {
            request_id: Some(Uuid::new_v4().to_string()),
            node_id: node_id.to_string(),
        };
        let resp = self
            .client
            .get(with_auth(req, &self.token))
            .await?
            .into_inner();

        Ok(resp.key_files)
    }

    pub async fn upload_keys(&mut self, node_id: &Uuid, keys: Vec<pb::Keyfile>) -> Result<()> {
        let req = pb::KeyFilesSaveRequest {
            request_id: Some(Uuid::new_v4().to_string()),
            node_id: node_id.to_string(),
            key_files: keys,
        };
        self.client.save(with_auth(req, &self.token)).await?;

        Ok(())
    }
}
