use crate::{
    services::api::AuthenticatedService,
    {config::SharedConfig, services, services::api::pb},
};
use bv_utils::with_retry;
use eyre::{Context, Result};
use tonic::transport::Endpoint;
use uuid::Uuid;

pub struct KeyService {
    client: pb::key_file_service_client::KeyFileServiceClient<AuthenticatedService>,
}

impl KeyService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        services::connect(config, |config| async {
            let url = config.read().await.blockjoy_api_url;
            let endpoint = Endpoint::from_shared(url.clone())?;
            let channel = Endpoint::connect(&endpoint)
                .await
                .with_context(|| format!("Failed to connect to key service at {url}"))?;
            let client = pb::key_file_service_client::KeyFileServiceClient::with_interceptor(
                channel,
                config.token().await?,
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
