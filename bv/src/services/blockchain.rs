use crate::{
    api_with_retry,
    services::{
        api::{
            common,
            pb::{self, blockchain_service_client, image_service_client},
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

pub struct BlockchainService<C> {
    connector: C,
}

type BlockchainServiceClient =
    blockchain_service_client::BlockchainServiceClient<AuthenticatedService>;
type ImageServiceClient = image_service_client::ImageServiceClient<AuthenticatedService>;

impl<C: ApiServiceConnector + Clone> BlockchainService<C> {
    pub async fn new(connector: C) -> Result<Self> {
        Ok(Self { connector })
    }

    async fn connect_blockchain_service(
        &self,
    ) -> Result<
        ApiClient<
            BlockchainServiceClient,
            impl ApiServiceConnector,
            impl Fn(Channel, ApiInterceptor) -> BlockchainServiceClient + Clone,
        >,
    > {
        ApiClient::build(
            self.connector.clone(),
            blockchain_service_client::BlockchainServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "cannot connect to blockchain service")
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
        blockchain_key: &str,
        node_type: NodeType,
        network: String,
        software: String,
        version: Option<String>,
        build_version: Option<u64>,
    ) -> Result<String> {
        info!("Getting image id...");
        let blockchain = self.get_blockchain(blockchain_key).await?;
        if let Some(version) = &version {
            if blockchain
                .versions
                .iter()
                .any(|value| value.software_version == *version)
            {
                bail!("blockchain version `{version}` not found");
            }
        }
        let mut client = self.connect_image_service().await?;
        let node_type: common::NodeType = node_type.into();
        let req = pb::ImageServiceGetImageRequest {
            version_key: Some(common::VersionKey {
                blockchain_key: blockchain_key.to_string(),
                node_type: node_type.into(),
                network,
                software,
            }),
            org_id: None,
            software_version: version,
            build_version,
        };

        let resp = api_with_retry!(client, client.get_image(req.clone()))?.into_inner();

        Ok(resp.image.ok_or(anyhow!("image not found"))?.id)
    }

    #[instrument(skip(self))]
    pub async fn list_blockchain_versions(&mut self, blockchain_key: &str) -> Result<Vec<String>> {
        info!("Getting blockchain versions...");
        let blockchain = self.get_blockchain(blockchain_key).await?;
        blockchain
            .versions
            .into_iter()
            .map(|version| {
                Ok(format!(
                    "{}/{}",
                    version
                        .version_key
                        .ok_or_else(|| anyhow!("Missing blockchain_version_key"))?
                        .node_type(),
                    version.software_version
                ))
            })
            .collect::<Result<Vec<_>>>()
    }

    async fn get_blockchain(&mut self, name: &str) -> Result<pb::Blockchain> {
        let mut client = self.connect_blockchain_service().await?;
        let req = pb::BlockchainServiceListBlockchainsRequest {
            org_ids: vec![],
            offset: 0,
            limit: 2,
            search: Some(pb::BlockchainSearch {
                operator: common::SearchOperator::Or.into(),
                id: None,
                name: Some(name.to_string()),
                display_name: None,
            }),
            sort: vec![],
        };

        let mut blockchains =
            api_with_retry!(client, client.list_blockchains(req.clone()))?.into_inner();

        if blockchains.blockchain_count > 1 {
            bail!("multiple blockchains found with the same name");
        }
        blockchains
            .blockchains
            .pop()
            .ok_or(anyhow!("blockchain with name `{name}` not found"))
    }
}

impl From<NodeType> for common::NodeType {
    fn from(value: NodeType) -> Self {
        match value {
            NodeType::Rpc => common::NodeType::Rpc,
        }
    }
}

impl TryFrom<common::NodeType> for NodeType {
    type Error = eyre::Error;
    fn try_from(value: common::NodeType) -> Result<Self, Self::Error> {
        Ok(match value {
            common::NodeType::Unspecified => bail!("Invalid Protocol"),
            common::NodeType::Rpc => NodeType::Rpc,
        })
    }
}
