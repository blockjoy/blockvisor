use crate::protocol::UiType;
use crate::{
    api_with_retry, protocol,
    services::{
        api::{
            common,
            pb::{self, image_service_client, protocol_service_client},
        },
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
};
use eyre::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tonic::transport::Channel;
use tracing::instrument;

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
    pub async fn get_image(
        &mut self,
        image_key: protocol::ImageKey,
        version: Option<String>,
        build_version: Option<u64>,
    ) -> Result<Option<pb::Image>> {
        let mut client = self.connect_image_service().await?;
        let req = pb::ImageServiceGetImageRequest {
            version_key: Some(image_key.into()),
            org_id: None,
            semantic_version: version,
            build_version,
        };

        let resp = api_with_retry!(client, client.get_image(req.clone()))?.into_inner();

        Ok(resp.image)
    }

    #[instrument(skip(self))]
    pub async fn get_protocol_version(
        &mut self,
        image_key: protocol::ImageKey,
    ) -> Result<Option<pb::ProtocolVersion>> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceGetLatestRequest {
            version_key: Some(image_key.into()),
            org_id: None,
        };
        let resp = api_with_retry!(client, client.get_latest(req.clone()))?.into_inner();
        Ok(resp.protocol_version)
    }

    pub async fn get_protocol_by_name(&mut self, name: &str) -> Result<Vec<pb::Protocol>> {
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

        Ok(api_with_retry!(client, client.list_protocols(req.clone()))?
            .into_inner()
            .protocols)
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
            .protocols)
    }

    pub async fn update_protocol(
        &mut self,
        protocol_id: String,
        protocol: protocol::Protocol,
    ) -> Result<()> {
        let mut client = self.connect_protocol_service().await?;
        let visibility: common::Visibility = protocol.visibility.into();
        let req = pb::ProtocolServiceUpdateProtocolRequest {
            protocol_id,
            visibility: Some(visibility.into()),
            description: protocol.description,
            name: Some(protocol.name),
        };
        api_with_retry!(client, client.update_protocol(req.clone()))?;
        Ok(())
    }

    pub async fn add_protocol(&mut self, protocol: protocol::Protocol) -> Result<()> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceAddProtocolRequest {
            org_id: protocol.org_id,
            key: protocol.key,
            name: protocol.name,
            description: protocol.description,
            ticker: protocol.ticker,
        };

        let resp = api_with_retry!(client, client.add_protocol(req.clone()))?
            .into_inner()
            .protocol
            .ok_or(anyhow!("missing protocol in response"))?;
        let visibility: common::Visibility = protocol.visibility.into();
        let req = pb::ProtocolServiceUpdateProtocolRequest {
            protocol_id: resp.protocol_id,
            visibility: Some(visibility.into()),
            description: None,
            name: None,
        };
        api_with_retry!(client, client.update_protocol(req.clone()))?;
        Ok(())
    }

    pub async fn update_protocol_version(
        &mut self,
        version_id: String,
        image: protocol::Image,
    ) -> Result<()> {
        let mut client = self.connect_protocol_service().await?;
        let visibility: common::Visibility = image.visibility.into();
        let req = pb::ProtocolServiceUpdateVersionRequest {
            protocol_version_id: version_id,
            visibility: Some(visibility.into()),
            description: image.description,
            sku_code: Some(image.sku_code),
        };
        api_with_retry!(client, client.update_version(req.clone()))?;
        Ok(())
    }

    pub async fn add_protocol_version(
        &mut self,
        image: protocol::Image,
    ) -> Result<pb::ProtocolVersion> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceAddVersionRequest {
            org_id: image.org_id,
            version_key: Some(image.key.into()),
            semantic_version: image.version,
            sku_code: image.sku_code,
            description: image.description,
        };

        let resp = api_with_retry!(client, client.add_version(req.clone()))?
            .into_inner()
            .version
            .ok_or(anyhow!("missing version in response"))?;
        let visibility: common::Visibility = image.visibility.into();
        let req = pb::ProtocolServiceUpdateVersionRequest {
            protocol_version_id: resp.protocol_version_id.clone(),
            visibility: Some(visibility.into()),
            description: None,
            sku_code: None,
        };
        api_with_retry!(client, client.update_version(req.clone()))?;
        Ok(resp)
    }

    pub async fn update_image(&mut self, image_id: String, image: protocol::Image) -> Result<()> {
        let mut client = self.connect_image_service().await?;
        let visibility: common::Visibility = image.visibility.into();
        let req = pb::ImageServiceUpdateImageRequest {
            image_id,
            visibility: Some(visibility.into()),
        };
        api_with_retry!(client, client.update_image(req.clone()))?;
        Ok(())
    }

    pub async fn add_image(
        &mut self,
        protocol_version_id: String,
        image: protocol::Image,
    ) -> Result<()> {
        let mut client = self.connect_image_service().await?;
        let req = pb::ImageServiceAddImageRequest {
            protocol_version_id,
            org_id: image.org_id,
            description: image.description,
            properties: add_properties(image.properties),
            firewall: Some(image.firewall_config.into()),
            min_cpu_cores: image.min_cpu,
            min_memory_bytes: image.min_memory_bytes,
            min_disk_bytes: image.min_disk_bytes,
            image_uri: image.container_uri,
            ramdisks: image
                .ramdisks
                .into_iter()
                .map(|ramdisk| ramdisk.into())
                .collect(),
        };

        let resp = api_with_retry!(client, client.add_image(req.clone()))?
            .into_inner()
            .image
            .ok_or(anyhow!("missing image in response"))?;
        let visibility: common::Visibility = image.visibility.into();
        let req = pb::ImageServiceUpdateImageRequest {
            image_id: resp.image_id,
            visibility: Some(visibility.into()),
        };
        api_with_retry!(client, client.update_image(req.clone()))?;
        Ok(())
    }
}

impl From<protocol::Visibility> for common::Visibility {
    fn from(value: protocol::Visibility) -> Self {
        match value {
            protocol::Visibility::Private => common::Visibility::Private,
            protocol::Visibility::Development => common::Visibility::Development,
            protocol::Visibility::Public => common::Visibility::Public,
        }
    }
}

impl From<protocol::ImageKey> for common::ProtocolVersionKey {
    fn from(value: protocol::ImageKey) -> Self {
        Self {
            protocol_key: value.protocol_key,
            variant_key: value.variant_key,
        }
    }
}

impl From<protocol::Action> for common::FirewallAction {
    fn from(value: protocol::Action) -> Self {
        match value {
            protocol::Action::Allow => common::FirewallAction::Allow,
            protocol::Action::Deny => common::FirewallAction::Drop,
            protocol::Action::Reject => common::FirewallAction::Reject,
        }
    }
}

impl From<protocol::NetProtocol> for common::FirewallProtocol {
    fn from(value: protocol::NetProtocol) -> Self {
        match value {
            protocol::NetProtocol::Tcp => common::FirewallProtocol::Tcp,
            protocol::NetProtocol::Udp => common::FirewallProtocol::Udp,
            protocol::NetProtocol::Both => common::FirewallProtocol::Both,
        }
    }
}

impl From<protocol::Direction> for common::FirewallDirection {
    fn from(value: protocol::Direction) -> Self {
        match value {
            protocol::Direction::In => common::FirewallDirection::Inbound,
            protocol::Direction::Out => common::FirewallDirection::Outbound,
        }
    }
}

impl From<protocol::FirewallRule> for common::FirewallRule {
    fn from(value: protocol::FirewallRule) -> Self {
        let action: common::FirewallAction = value.action.into();
        let direction: common::FirewallDirection = value.direction.into();
        let protocol: common::FirewallProtocol = value.protocol.into();

        Self {
            key: value.key,
            action: action.into(),
            direction: direction.into(),
            protocol: protocol.into(),
            ips: value
                .ips
                .into_iter()
                .map(|ip| common::IpName {
                    ip: ip.ip,
                    name: ip.name,
                })
                .collect(),
            ports: value
                .ports
                .into_iter()
                .map(|port| common::PortName {
                    port: port.port as u32,
                    name: port.name,
                })
                .collect(),
            description: value.description,
        }
    }
}

impl From<protocol::FirewallConfig> for common::FirewallConfig {
    fn from(value: protocol::FirewallConfig) -> Self {
        let default_in: common::FirewallAction = value.default_in.into();
        let default_out: common::FirewallAction = value.default_out.into();
        Self {
            default_in: default_in.into(),
            default_out: default_out.into(),
            rules: value.rules.into_iter().map(|rule| rule.into()).collect(),
        }
    }
}

impl From<protocol::RamdiskConfig> for common::RamdiskConfig {
    fn from(value: protocol::RamdiskConfig) -> Self {
        Self {
            mount: value.mount,
            size_bytes: value.size_bytes,
        }
    }
}

fn add_properties(image_properties: Vec<protocol::ImageProperty>) -> Vec<pb::AddImageProperty> {
    let mut add_properties = vec![];
    for property in image_properties {
        match property.ui_type {
            UiType::Text(impact) | UiType::Password(impact) => {
                add_properties.push(pb::AddImageProperty {
                    key: property.key,
                    description: property.description,
                    new_archive: impact
                        .as_ref()
                        .map(|impact| impact.new_archive)
                        .unwrap_or_default(),
                    key_default: None,
                    default_value: property.default_value,
                    dynamic_value: property.dynamic_value,
                    ui_type: common::UiType::Text.into(),
                    add_cpu_cores: impact.as_ref().and_then(|impact| impact.add_cpu),
                    add_memory_bytes: impact.as_ref().and_then(|impact| impact.add_memory_bytes),
                    add_disk_bytes: impact.as_ref().and_then(|impact| impact.add_disk_bytes),
                })
            }
            UiType::Switch { on, off } => {
                add_properties.push(pb::AddImageProperty {
                    key: property.key.clone(),
                    description: property.description.clone(),
                    new_archive: on
                        .impact
                        .as_ref()
                        .map(|impact| impact.new_archive)
                        .unwrap_or_default(),
                    key_default: Some(property.default_value == on.value),
                    default_value: on.value,
                    dynamic_value: property.dynamic_value,
                    ui_type: common::UiType::Switch.into(),
                    add_cpu_cores: on.impact.as_ref().and_then(|impact| impact.add_cpu),
                    add_memory_bytes: on
                        .impact
                        .as_ref()
                        .and_then(|impact| impact.add_memory_bytes),
                    add_disk_bytes: on.impact.as_ref().and_then(|impact| impact.add_disk_bytes),
                });
                add_properties.push(pb::AddImageProperty {
                    key: property.key,
                    description: property.description,
                    new_archive: off
                        .impact
                        .as_ref()
                        .map(|impact| impact.new_archive)
                        .unwrap_or_default(),
                    key_default: Some(property.default_value == off.value),
                    default_value: off.value,
                    dynamic_value: property.dynamic_value,
                    ui_type: common::UiType::Switch.into(),
                    add_cpu_cores: off.impact.as_ref().and_then(|impact| impact.add_cpu),
                    add_memory_bytes: off
                        .impact
                        .as_ref()
                        .and_then(|impact| impact.add_memory_bytes),
                    add_disk_bytes: off.impact.as_ref().and_then(|impact| impact.add_disk_bytes),
                });
            }
            UiType::Enum(variants) => {
                for variant in variants {
                    add_properties.push(pb::AddImageProperty {
                        key: property.key.clone(),
                        description: property.description.clone(),
                        new_archive: variant
                            .impact
                            .as_ref()
                            .map(|impact| impact.new_archive)
                            .unwrap_or_default(),
                        key_default: Some(property.default_value == variant.value),
                        default_value: variant.value,
                        dynamic_value: property.dynamic_value,
                        ui_type: common::UiType::Enum.into(),
                        add_cpu_cores: variant.impact.as_ref().and_then(|impact| impact.add_cpu),
                        add_memory_bytes: variant
                            .impact
                            .as_ref()
                            .and_then(|impact| impact.add_memory_bytes),
                        add_disk_bytes: variant
                            .impact
                            .as_ref()
                            .and_then(|impact| impact.add_disk_bytes),
                    })
                }
            }
        }
    }
    add_properties
}
