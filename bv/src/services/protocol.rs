use crate::nib_meta::StorePointer;
use crate::{
    api_with_retry,
    nib_meta::{self, UiType},
    node_state::ProtocolImageKey,
    services::{
        api::{
            common,
            pb::{self, image_service_client, protocol_service_client},
        },
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
};
use core::fmt;
use eyre::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Formatter;
use tonic::transport::Channel;
use tracing::instrument;

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq)]
pub enum NodeType {
    Rpc,
}

pub enum PushResult<T> {
    Added(T),
    Updated(T),
    NoChanges,
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
        image_key: ProtocolImageKey,
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
        match api_with_retry!(client, client.get_image(req.clone())) {
            Ok(resp) => Ok(resp.into_inner().image),
            Err(err) if err.code() == tonic::Code::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    #[instrument(skip(self))]
    pub async fn get_protocol_version(
        &mut self,
        image_key: ProtocolImageKey,
    ) -> Result<Option<pb::ProtocolVersion>> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceGetLatestRequest {
            version_key: Some(image_key.into()),
            org_id: None,
        };
        match api_with_retry!(client, client.get_latest(req.clone())) {
            Ok(resp) => Ok(resp.into_inner().protocol_version),
            Err(err) if err.code() == tonic::Code::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn get_protocol(&mut self, key: String) -> Result<Option<pb::Protocol>> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceGetProtocolRequest {
            org_id: None,
            protocol: Some(pb::protocol_service_get_protocol_request::Protocol::ProtocolKey(key)),
        };

        match api_with_retry!(client, client.get_protocol(req.clone())) {
            Ok(resp) => Ok(resp.into_inner().protocol),
            Err(err) if err.code() == tonic::Code::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn list_protocols(
        &mut self,
        name: Option<String>,
        limit: u64,
    ) -> Result<Vec<pb::Protocol>> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceListProtocolsRequest {
            org_ids: vec![],
            offset: 0,
            limit,
            search: name.map(|name| pb::ProtocolSearch {
                operator: common::SearchOperator::Or.into(),
                protocol_id: None,
                name: Some(format!("%{name}%")),
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
        protocol: nib_meta::Protocol,
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

    pub async fn add_protocol(&mut self, protocol: nib_meta::Protocol) -> Result<()> {
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
        remote: pb::ProtocolVersion,
        image: nib_meta::Image,
    ) -> Result<()> {
        let mut client = self.connect_protocol_service().await?;
        let visibility: common::Visibility = image.visibility.into();
        if remote.visibility() != visibility
            || remote.description != image.description
            || remote.sku_code != image.sku_code
        {
            let req = pb::ProtocolServiceUpdateVersionRequest {
                protocol_version_id: remote.protocol_version_id,
                visibility: Some(visibility.into()),
                description: image.description,
                sku_code: Some(image.sku_code),
            };
            api_with_retry!(client, client.update_version(req.clone()))?;
        }
        Ok(())
    }

    pub async fn add_protocol_version(
        &mut self,
        image: nib_meta::Image,
        variant: nib_meta::Variant,
    ) -> Result<pb::ProtocolVersion> {
        let mut client = self.connect_protocol_service().await?;
        let req = pb::ProtocolServiceAddVersionRequest {
            org_id: image.org_id,
            version_key: Some(common::ProtocolVersionKey {
                protocol_key: image.protocol_key,
                variant_key: variant.key,
            }),
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

    pub async fn push_image(
        &mut self,
        protocol_version_id: String,
        image: nib_meta::Image,
        variant: nib_meta::Variant,
    ) -> Result<PushResult<pb::Image>> {
        let mut client = self.connect_image_service().await?;
        let mut firewall: common::FirewallConfig = image.firewall_config.into();
        firewall.rules.sort_by(|a, b| a.key.cmp(&b.key));
        let req = pb::ImageServiceAddImageRequest {
            protocol_version_id,
            org_id: image.org_id,
            description: image.description,
            properties: add_properties(image.properties),
            firewall: Some(firewall),
            min_cpu_cores: variant.min_cpu,
            min_memory_bytes: variant.min_memory_bytes,
            min_disk_bytes: variant.min_disk_bytes,
            image_uri: image.container_uri,
            ramdisks: variant
                .ramdisks
                .into_iter()
                .map(|ramdisk| ramdisk.into())
                .collect(),
            archive_pointers: variant
                .archive_pointers
                .into_iter()
                .map(|pointer| pointer.into())
                .collect(),
        };
        if let Some(mut remote) = self
            .get_image(
                ProtocolImageKey {
                    protocol_key: image.protocol_key,
                    variant_key: variant.key,
                },
                Some(image.version.clone()),
                None,
            )
            .await?
        {
            let remote_visibility = remote.visibility();
            // Need to sort properties and firewall rules, so it can be reliably compared.
            let mut local_properties = req.properties.clone();
            local_properties.sort_by(|a, b| a.key.cmp(&b.key));
            let mut remote_properties = remote
                .properties
                .into_iter()
                .map(|property| pb::AddImageProperty {
                    key: property.key,
                    key_group: property.key_group,
                    description: property.description,
                    new_archive: property.new_archive,
                    default_value: property.default_value,
                    dynamic_value: property.dynamic_value,
                    ui_type: property.ui_type,
                    add_cpu_cores: property.add_cpu_cores,
                    add_memory_bytes: property.add_memory_bytes,
                    add_disk_bytes: property.add_disk_bytes,
                    is_group_default: property.is_group_default,
                })
                .collect::<Vec<_>>();
            remote_properties.sort_by(|a, b| a.key.cmp(&b.key));
            if let Some(firewall) = &mut remote.firewall {
                firewall.rules.sort_by(|a, b| a.key.cmp(&b.key));
            }
            // Update only if everything match, but visibility.
            if req.org_id == remote.org_id
                && req.description == remote.description
                && local_properties == remote_properties
                && req.firewall == remote.firewall
                && req.min_cpu_cores == remote.min_cpu_cores
                && req.min_memory_bytes == remote.min_memory_bytes
                && req.min_disk_bytes == remote.min_disk_bytes
                && req.image_uri == remote.image_uri
                && req.ramdisks == remote.ramdisks
            {
                let visibility: common::Visibility = image.visibility.into();
                return Ok(if visibility != remote_visibility {
                    let req = pb::ImageServiceUpdateImageRequest {
                        image_id: remote.image_id,
                        visibility: Some(visibility.into()),
                    };
                    PushResult::Updated(
                        api_with_retry!(client, client.update_image(req.clone()))?
                            .into_inner()
                            .image
                            .ok_or(anyhow!("missing image in response"))?,
                    )
                } else {
                    PushResult::NoChanges
                });
            }
        }
        // Otherwise, add new image build.
        let resp = api_with_retry!(client, client.add_image(req.clone()))?
            .into_inner()
            .image
            .ok_or(anyhow!("missing image in response"))?;
        let visibility: common::Visibility = image.visibility.into();
        let req = pb::ImageServiceUpdateImageRequest {
            image_id: resp.image_id.clone(),
            visibility: Some(visibility.into()),
        };
        api_with_retry!(client, client.update_image(req.clone()))?;
        Ok(PushResult::Added(resp))
    }
}

impl From<nib_meta::Visibility> for common::Visibility {
    fn from(value: nib_meta::Visibility) -> Self {
        match value {
            nib_meta::Visibility::Private => common::Visibility::Private,
            nib_meta::Visibility::Development => common::Visibility::Development,
            nib_meta::Visibility::Public => common::Visibility::Public,
        }
    }
}

impl From<ProtocolImageKey> for common::ProtocolVersionKey {
    fn from(value: ProtocolImageKey) -> Self {
        Self {
            protocol_key: value.protocol_key,
            variant_key: value.variant_key,
        }
    }
}

impl From<nib_meta::Action> for common::FirewallAction {
    fn from(value: nib_meta::Action) -> Self {
        match value {
            nib_meta::Action::Allow => common::FirewallAction::Allow,
            nib_meta::Action::Deny => common::FirewallAction::Drop,
            nib_meta::Action::Reject => common::FirewallAction::Reject,
        }
    }
}

impl From<nib_meta::NetProtocol> for common::FirewallProtocol {
    fn from(value: nib_meta::NetProtocol) -> Self {
        match value {
            nib_meta::NetProtocol::Tcp => common::FirewallProtocol::Tcp,
            nib_meta::NetProtocol::Udp => common::FirewallProtocol::Udp,
            nib_meta::NetProtocol::Both => common::FirewallProtocol::Both,
        }
    }
}

impl From<nib_meta::Direction> for common::FirewallDirection {
    fn from(value: nib_meta::Direction) -> Self {
        match value {
            nib_meta::Direction::In => common::FirewallDirection::Inbound,
            nib_meta::Direction::Out => common::FirewallDirection::Outbound,
        }
    }
}

impl From<nib_meta::FirewallRule> for common::FirewallRule {
    fn from(value: nib_meta::FirewallRule) -> Self {
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

impl From<nib_meta::FirewallConfig> for common::FirewallConfig {
    fn from(value: nib_meta::FirewallConfig) -> Self {
        let default_in: common::FirewallAction = value.default_in.into();
        let default_out: common::FirewallAction = value.default_out.into();
        Self {
            default_in: default_in.into(),
            default_out: default_out.into(),
            rules: value.rules.into_iter().map(|rule| rule.into()).collect(),
        }
    }
}

impl From<nib_meta::RamdiskConfig> for common::RamdiskConfig {
    fn from(value: nib_meta::RamdiskConfig) -> Self {
        Self {
            mount: value.mount,
            size_bytes: value.size_bytes,
        }
    }
}

impl From<nib_meta::ArchivePointer> for pb::ArchivePointer {
    fn from(value: nib_meta::ArchivePointer) -> Self {
        Self {
            new_archive_keys: value.new_archive_properties,
            pointer: Some(match value.pointer {
                StorePointer::CombinationDisallowed => pb::archive_pointer::Pointer::Disallowed(()),
                StorePointer::StoreId(id) => pb::archive_pointer::Pointer::StoreId(id),
            }),
        }
    }
}

fn add_properties(image_properties: Vec<nib_meta::ImageProperty>) -> Vec<pb::AddImageProperty> {
    let mut add_properties = vec![];
    for property in image_properties {
        match property.ui_type {
            UiType::Text(impact) | UiType::Password(impact) => {
                add_properties.push(pb::AddImageProperty {
                    key: property.key,
                    key_group: None,
                    description: property.description,
                    new_archive: impact
                        .as_ref()
                        .map(|impact| impact.new_archive)
                        .unwrap_or_default(),
                    default_value: property.default_value,
                    dynamic_value: property.dynamic_value,
                    ui_type: common::UiType::Text.into(),
                    add_cpu_cores: impact.as_ref().and_then(|impact| impact.add_cpu),
                    add_memory_bytes: impact.as_ref().and_then(|impact| impact.add_memory_bytes),
                    add_disk_bytes: impact.as_ref().and_then(|impact| impact.add_disk_bytes),
                    is_group_default: None,
                })
            }
            UiType::Switch { on, off } => {
                add_properties.push(pb::AddImageProperty {
                    key: format!("{}-{}", property.key, on.value),
                    key_group: Some(property.key.clone()),
                    description: property.description.clone(),
                    new_archive: on
                        .impact
                        .as_ref()
                        .map(|impact| impact.new_archive)
                        .unwrap_or_default(),
                    is_group_default: Some(property.default_value == on.value),
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
                    key: format!("{}-{}", property.key, off.value),
                    key_group: Some(property.key.clone()),
                    description: property.description,
                    new_archive: off
                        .impact
                        .as_ref()
                        .map(|impact| impact.new_archive)
                        .unwrap_or_default(),
                    is_group_default: Some(property.default_value == off.value),
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
                        key: format!("{}-{}", property.key, variant.value),
                        key_group: Some(property.key.clone()),
                        description: property.description.clone(),
                        new_archive: variant
                            .impact
                            .as_ref()
                            .map(|impact| impact.new_archive)
                            .unwrap_or_default(),
                        is_group_default: Some(property.default_value == variant.value),
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

impl fmt::Display for pb::Protocol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(description) = &self.description {
            writeln!(f, "- {} [{}]: {}", self.name, self.key, description)?;
        } else {
            writeln!(f, "- {} ({})", self.name, self.key)?;
        }
        for version in &self.versions {
            if let Some(variant) = &version.version_key {
                writeln!(
                    f,
                    "   * {}/{}",
                    variant.variant_key, version.semantic_version
                )?;
            }
        }
        Ok(())
    }
}
