use crate::{
    services::AuthenticatedService,
    {config::SharedConfig, services, services::api::pb},
};
use bv_utils::with_retry;
use eyre::Result;
use uuid::Uuid;

pub struct KeyService {
    client: pb::key_file_service_client::KeyFileServiceClient<AuthenticatedService>,
}

impl KeyService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        Ok(Self {
            client: services::connect_to_api_service(
                config,
                pb::key_file_service_client::KeyFileServiceClient::with_interceptor,
            )
            .await?,
        })
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
