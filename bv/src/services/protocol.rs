use crate::{
    api_with_retry,
    services::{
        api::{
            common,
            pb::{self, image_service_client, protocol_service_client},
        },
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
};
use eyre::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tonic::transport::Channel;
use tracing::{info, instrument};

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq)]
pub enum NodeType {
    Rpc,
}

pub struct ProtocolService<C> {
    connector: C,
}

type ProtocolServiceClient = protocol_service_client::ProtocolServiceClient<AuthenticatedService>;
type ImageServiceClient = image_service_client::ImageServiceClient<AuthenticatedService>;

impl<C: ApiServiceConnector + Clone> ProtocolService<C> {
    pub async fn new(connector: C) -> Result<Self> {
        Ok(Self { connector })
    }

    async fn connect_protocol_service(
        &self,
    ) -> Result<
        ApiClient<
            ProtocolServiceClient,
            impl ApiServiceConnector,
            impl Fn(Channel, ApiInterceptor) -> ProtocolServiceClient + Clone,
        >,
    > {
        ApiClient::build(
            self.connector.clone(),
            protocol_service_client::ProtocolServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "cannot connect to protocol service")
    }

    async fn connect_image_service(
        &self,
    ) -> Result<
        ApiClient<
            ImageServiceClient,
            impl ApiServiceConnector,
            impl Fn(Channel, ApiInterceptor) -> ImageServiceClient + Clone,
        >,
    > {
        ApiClient::build(
            self.connector.clone(),
            image_service_client::ImageServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "cannot connect to image service")
    }

    #[instrument(skip(self))]
    pub async fn get_image_id(
        &mut self,
        protocol_key: &str,
        variant_key: &str,
        version: Option<String>,
        build_version: Option<u64>,
    ) -> Result<String> {
        info!("Getting image id...");
        let protocol = self.get_protocol(protocol_key).await?;
        if let Some(version) = &version {
            if protocol
                .versions
                .iter()
                .any(|value| value.semantic_version == *version)
            {
                bail!("protocol version `{version}` not found");
            }
        }
        let mut client = self.connect_image_service().await?;
        let req = pb::ImageServiceGetImageRequest {
            version_key: Some(common::ProtocolVersionKey {
                protocol_key: protocol_key.to_string(),
                variant_key: variant_key.to_string(),
            }),
            org_id: None,
            protocol_version: version,
            build_version,
        };

        let resp = api_with_retry!(client, client.get_image(req.clone()))?.into_inner();

        Ok(resp.image.ok_or(anyhow!("image not found"))?.image_id)
    }

    #[instrument(skip(self))]
    pub async fn list_protocol_images(&mut self, protocol_key: &str) -> Result<Vec<String>> {
        info!("Getting protocol versions...");
        let protocol = self.get_protocol(protocol_key).await?;
        protocol
            .versions
            .into_iter()
            .map(|version| {
                Ok(format!(
                    "{}/{}",
                    version
                        .version_key
                        .ok_or_else(|| anyhow!("Missing version_key"))?
                        .variant_key,
                    version.semantic_version
                ))
            })
            .collect::<Result<Vec<_>>>()
    }

    async fn get_protocol(&mut self, name: &str) -> Result<pb::Protocol> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceListProtocolsRequest {
            org_ids: vec![],
            offset: 0,
            limit: 2,
            search: Some(pb::ProtocolSearch {
                operator: common::SearchOperator::Or.into(),
                protocol_id: None,
                name: Some(name.to_string()),
            }),
            sort: vec![],
        };

        let mut protocols =
            api_with_retry!(client, client.list_protocols(req.clone()))?.into_inner();

        if protocols.matches.len() > 1 {
            bail!("multiple protocols found with the same key");
        }
        protocols
            .matches
            .pop()
            .ok_or(anyhow!("protocol with name `{name}` not found"))
    }

    pub async fn list_protocols(&mut self) -> Result<Vec<pb::Protocol>> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceListProtocolsRequest {
            org_ids: vec![],
            offset: 0,
            limit: 0,
            search: Some(pb::ProtocolSearch {
                operator: common::SearchOperator::Or.into(),
                protocol_id: None,
                name: None,
            }),
            sort: vec![pb::ProtocolSort {
                field: pb::ProtocolSortField::Name.into(),
                order: common::SortOrder::Ascending.into(),
            }],
        };

        Ok(api_with_retry!(client, client.list_protocols(req.clone()))?
            .into_inner()
            .matches)
    }
}
