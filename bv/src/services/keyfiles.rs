use crate::{
    services::api::{AuthToken, AuthenticatedService},
    {config::SharedConfig, services, services::api::pb, with_retry},
};
use anyhow::{anyhow, Context, Result};
use tonic::transport::Endpoint;
use uuid::Uuid;

pub struct KeyService {
    client: pb::key_file_service_client::KeyFileServiceClient<AuthenticatedService>,
}

impl KeyService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        services::connect(config.clone(), |config| async {
            let url = config
                .blockjoy_keys_url
                .ok_or_else(|| anyhow!("missing blockjoy_keys_url"))?;
            let endpoint = Endpoint::from_shared(url.clone())?;
            let client = pb::key_file_service_client::KeyFileServiceClient::with_interceptor(
                Endpoint::connect(&endpoint)
                    .await
                    .context(format!("Failed to connect to key service at {url}"))?,
                AuthToken(config.token),
            );
            Ok(Self { client })
        })
        .await
    }

    pub async fn download_keys(&mut self, node_id: Uuid) -> Result<Vec<pb::Keyfile>> {
        let req = pb::KeyFileServiceListRequest {
            node_id: node_id.to_string(),
        };
        let resp = with_retry!(self.client.list(req.clone()))?.into_inner();

        Ok(resp.key_files)
    }

    pub async fn upload_keys(&mut self, node_id: Uuid, keys: Vec<pb::Keyfile>) -> Result<()> {
        let req = pb::KeyFileServiceCreateRequest {
            node_id: node_id.to_string(),
            key_files: keys,
        };
        with_retry!(self.client.create(req.clone()))?;

        Ok(())
    }
}
