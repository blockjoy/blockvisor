use crate::{
    command_failed, commands, commands::Error, config::SharedConfig, get_bv_status,
    node_data::NodeImage, nodes_manager, nodes_manager::NodesManager, pal::Pal, services,
    services::AuthenticatedService, ServiceStatus,
};
use babel_api::engine::Slot;
use babel_api::{
    engine::{Checksum, Chunk, Compression, DownloadManifest, FileLocation, UploadManifest},
    metadata::firewall,
};
use bv_utils::with_retry;
use eyre::{anyhow, bail, Context, Result};
use metrics::{register_counter, Counter};
use pb::{
    blockchain_archive_service_client, blockchain_service_client,
    command_service_client::CommandServiceClient, discovery_service_client, host_service_client,
    node_command::Command, node_service_client,
};
use reqwest::Url;
use std::{
    fmt::Debug,
    path::PathBuf,
    {str::FromStr, sync::Arc},
};
use tokio::time::Instant;
use tracing::{error, info, instrument};
use uuid::Uuid;

#[allow(clippy::large_enum_variant)]
pub mod pb {
    tonic::include_proto!("blockjoy.v1");
}

#[allow(clippy::large_enum_variant)]
pub mod common {
    tonic::include_proto!("blockjoy.common.v1");

    pub mod v1 {
        pub use super::*;
    }
}

lazy_static::lazy_static! {
    pub static ref API_CREATE_COUNTER: Counter = register_counter!("api.commands.create.calls");
    pub static ref API_CREATE_TIME_MS_COUNTER: Counter = register_counter!("api.commands.create.ms");
    pub static ref API_DELETE_COUNTER: Counter = register_counter!("api.commands.delete.calls");
    pub static ref API_DELETE_TIME_MS_COUNTER: Counter = register_counter!("api.commands.delete.ms");
    pub static ref API_START_COUNTER: Counter = register_counter!("api.commands.start.calls");
    pub static ref API_START_TIME_MS_COUNTER: Counter = register_counter!("api.commands.start.ms");
    pub static ref API_STOP_COUNTER: Counter = register_counter!("api.commands.stop.calls");
    pub static ref API_STOP_TIME_MS_COUNTER: Counter = register_counter!("api.commands.stop.ms");
    pub static ref API_RESTART_COUNTER: Counter = register_counter!("api.commands.restart.calls");
    pub static ref API_RESTART_TIME_MS_COUNTER: Counter = register_counter!("api.commands.restart.ms");
    pub static ref API_UPGRADE_COUNTER: Counter = register_counter!("api.commands.upgrade.calls");
    pub static ref API_UPGRADE_TIME_MS_COUNTER: Counter = register_counter!("api.commands.upgrade.ms");
    pub static ref API_UPDATE_COUNTER: Counter = register_counter!("api.commands.update.calls");
    pub static ref API_UPDATE_TIME_MS_COUNTER: Counter = register_counter!("api.commands.update.ms");
}

pub type BlockchainServiceClient =
    blockchain_service_client::BlockchainServiceClient<AuthenticatedService>;
pub type BlockchainArchiveServiceClient =
    blockchain_archive_service_client::BlockchainArchiveServiceClient<AuthenticatedService>;
pub type DiscoveryServiceClient =
    discovery_service_client::DiscoveryServiceClient<AuthenticatedService>;
pub type HostsServiceClient = host_service_client::HostServiceClient<AuthenticatedService>;
pub type NodesServiceClient = node_service_client::NodeServiceClient<AuthenticatedService>;

pub struct CommandsService {
    client: CommandServiceClient<AuthenticatedService>,
}

impl CommandsService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        Ok(Self {
            client: services::connect_to_api_service(
                config,
                CommandServiceClient::with_interceptor,
            )
            .await?,
        })
    }

    pub async fn get_and_process_pending_commands<P: Pal + Debug>(
        &mut self,
        host_id: &str,
        nodes_manager: Arc<NodesManager<P>>,
    ) -> Result<()> {
        let commands = self
            .get_pending_commands(host_id)
            .await
            .with_context(|| "cannot get pending commands")?;
        self.process_commands(commands, nodes_manager)
            .await
            .with_context(|| "cannot process commands")?;
        Ok(())
    }

    pub async fn get_pending_commands(&mut self, host_id: &str) -> Result<Vec<pb::Command>> {
        info!("Get pending commands");

        let req = pb::CommandServicePendingRequest {
            host_id: host_id.to_string(),
            filter_type: None,
        };
        let resp = with_retry!(self.client.pending(req.clone()))?.into_inner();

        Ok(resp.commands)
    }

    pub async fn process_commands<P: Pal + Debug>(
        &mut self,
        commands: Vec<pb::Command>,
        nodes_manager: Arc<NodesManager<P>>,
    ) -> Result<()> {
        info!("Processing {} commands", commands.len());

        for command in commands {
            info!("Processing command: {command:?}");

            match command.command {
                Some(pb::command::Command::Node(node_command)) => {
                    let command_id = command.id.clone();
                    // check for bv health status
                    let service_status = get_bv_status().await;
                    if service_status != ServiceStatus::Ok {
                        self.send_service_status_update(command_id.clone(), service_status)
                            .await
                            .with_context(|| "cannot send system status update")?;
                    } else {
                        self.send_command_ack(command_id.clone())
                            .await
                            .with_context(|| "cannot ack command")?;
                        // process the command
                        let command_result =
                            process_node_command(nodes_manager.clone(), node_command).await;
                        if let Err(error) = &command_result {
                            error!("Error processing command: {error:?}");
                        }
                        self.send_command_update(command_id, command_result).await?;
                    }
                }
                Some(pb::command::Command::Host(_)) => {
                    error!("Error processing command: Command type `Host` not supported");
                    let command_id = command.id;
                    self.send_command_ack(command_id.clone()).await?;
                    self.send_command_update(command_id, Err(Error::NotSupported))
                        .await?;
                }
                None => {
                    let msg = "Command type is `None`".to_string();
                    error!("Error processing command: {msg}");
                }
            };
        }

        Ok(())
    }

    /// Informing API that we have finished with the command.
    #[instrument(skip(self))]
    async fn send_command_update(
        &mut self,
        command_id: String,
        command_result: commands::Result<()>,
    ) -> Result<()> {
        let req = match command_result {
            Ok(()) => pb::CommandServiceUpdateRequest {
                id: command_id,
                exit_code: Some(pb::CommandExitCode::Ok.into()),
                exit_message: None,
                retry_hint_seconds: None,
            },
            Err(err) => {
                let mut req = pb::CommandServiceUpdateRequest {
                    id: command_id,
                    exit_code: Some(match &err {
                        Error::Internal(_) => pb::CommandExitCode::InternalError.into(),
                        Error::ServiceNotReady => pb::CommandExitCode::ServiceNotReady.into(),
                        Error::ServiceBroken => pb::CommandExitCode::ServiceBroken.into(),
                        Error::NotSupported => pb::CommandExitCode::NotSupported.into(),
                        Error::NodeNotFound(_) => pb::CommandExitCode::NodeNotFound.into(),
                        Error::BlockingJobRunning { .. } => {
                            pb::CommandExitCode::BlockingJobRunning.into()
                        }
                    }),
                    exit_message: Some(format!("{err:#}")),
                    retry_hint_seconds: None,
                };
                if let Error::BlockingJobRunning { retry_hint } = err {
                    req.retry_hint_seconds = Some(retry_hint.as_secs())
                }
                req
            }
        };
        with_retry!(self.client.update(req.clone()))?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn send_command_ack(&mut self, command_id: String) -> Result<()> {
        let req = pb::CommandServiceAckRequest { id: command_id };
        with_retry!(self.client.ack(req.clone()))?;
        Ok(())
    }

    async fn send_service_status_update(
        &mut self,
        command_id: String,
        status: ServiceStatus,
    ) -> Result<()> {
        match status {
            ServiceStatus::Undefined => {
                self.send_command_update(command_id, Err(commands::Error::ServiceNotReady))
                    .await
            }
            ServiceStatus::Updating => {
                self.send_command_update(command_id, Err(commands::Error::ServiceNotReady))
                    .await
            }
            ServiceStatus::Broken => {
                self.send_command_update(command_id, Err(commands::Error::ServiceBroken))
                    .await
            }
            ServiceStatus::Ok => Ok(()),
        }
    }
}

async fn process_node_command<P: Pal + Debug>(
    nodes_manager: Arc<NodesManager<P>>,
    node_command: pb::NodeCommand,
) -> commands::Result<()> {
    let node_id = Uuid::from_str(&node_command.node_id).map_err(|err| anyhow!(err))?;
    let now = Instant::now();
    match node_command.command {
        Some(cmd) => match cmd {
            Command::Create(args) => {
                let image: NodeImage = args
                    .image
                    .ok_or_else(|| anyhow!("Image not provided"))?
                    .into();
                let properties = args
                    .properties
                    .into_iter()
                    .map(|p| (p.name, p.value))
                    .chain([("network".to_string(), args.network.clone())])
                    .collect();
                let rules = args
                    .rules
                    .into_iter()
                    .map(|rule| rule.try_into())
                    .collect::<Result<Vec<_>>>()?;
                nodes_manager
                    .create(
                        node_id,
                        nodes_manager::NodeConfig {
                            name: args.name,
                            image,
                            ip: args.ip,
                            gateway: args.gateway,
                            rules,
                            properties,
                            network: args.network,
                            standalone: false,
                        },
                    )
                    .await?;
                API_CREATE_COUNTER.increment(1);
                API_CREATE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Delete(_) => {
                nodes_manager.delete(node_id).await?;
                API_DELETE_COUNTER.increment(1);
                API_DELETE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Start(_) => {
                nodes_manager.start(node_id, false).await?;
                API_START_COUNTER.increment(1);
                API_START_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Stop(_) => {
                nodes_manager.stop(node_id, false).await?;
                API_STOP_COUNTER.increment(1);
                API_STOP_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Restart(_) => {
                nodes_manager.stop(node_id, false).await?;
                nodes_manager.start(node_id, false).await?;
                API_RESTART_COUNTER.increment(1);
                API_RESTART_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Upgrade(args) => {
                let image: NodeImage = args
                    .image
                    .ok_or_else(|| anyhow!("Image not provided"))?
                    .into();
                nodes_manager.upgrade(node_id, image).await?;
                API_UPGRADE_COUNTER.increment(1);
                API_UPGRADE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Update(pb::NodeUpdate { rules }) => {
                nodes_manager
                    .update(
                        node_id,
                        rules
                            .into_iter()
                            .map(|rule| rule.try_into())
                            .collect::<Result<Vec<_>>>()?,
                    )
                    .await?;
                API_UPDATE_COUNTER.increment(1);
                API_UPDATE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::InfoGet(_) => unimplemented!(),
        },
        None => command_failed!(Error::Internal(anyhow!("Node command is `None`"))),
    };

    Ok(())
}

impl From<common::ImageIdentifier> for NodeImage {
    fn from(image: common::ImageIdentifier) -> Self {
        Self {
            protocol: image.protocol.to_lowercase(),
            node_type: image.node_type().to_string(),
            node_version: image.node_version.to_lowercase(),
        }
    }
}

impl TryFrom<NodeImage> for common::ImageIdentifier {
    type Error = eyre::Error;

    fn try_from(image: NodeImage) -> Result<Self, Self::Error> {
        Ok(Self {
            protocol: image.protocol,
            node_type: common::NodeType::from_str(&image.node_type)?.into(),
            node_version: image.node_version,
        })
    }
}

impl std::fmt::Display for common::NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.as_str_name();
        let s = s.strip_prefix("NODE_TYPE_").unwrap_or(s).to_lowercase();
        write!(f, "{s}")
    }
}

impl FromStr for common::NodeType {
    type Err = eyre::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let str = format!("NODE_TYPE_{}", s.to_uppercase());
        Self::from_str_name(&str).ok_or_else(|| anyhow!("Invalid NodeType {s}"))
    }
}

impl TryFrom<common::FirewallAction> for firewall::Action {
    type Error = eyre::Error;
    fn try_from(value: common::FirewallAction) -> Result<Self, Self::Error> {
        Ok(match value {
            common::FirewallAction::Unspecified => {
                bail!("Invalid Action")
            }
            common::FirewallAction::Allow => firewall::Action::Allow,
            common::FirewallAction::Deny => firewall::Action::Deny,
            common::FirewallAction::Reject => firewall::Action::Reject,
        })
    }
}

impl TryFrom<common::FirewallDirection> for firewall::Direction {
    type Error = eyre::Error;
    fn try_from(value: common::FirewallDirection) -> Result<Self, Self::Error> {
        Ok(match value {
            common::FirewallDirection::Unspecified => {
                bail!("Invalid Direction")
            }
            common::FirewallDirection::Inbound => firewall::Direction::In,
            common::FirewallDirection::Outbound => firewall::Direction::Out,
        })
    }
}

impl TryFrom<common::FirewallProtocol> for firewall::Protocol {
    type Error = eyre::Error;
    fn try_from(value: common::FirewallProtocol) -> Result<Self, Self::Error> {
        Ok(match value {
            common::FirewallProtocol::Unspecified => bail!("Invalid Protocol"),
            common::FirewallProtocol::Tcp => firewall::Protocol::Tcp,
            common::FirewallProtocol::Udp => firewall::Protocol::Udp,
            common::FirewallProtocol::Both => firewall::Protocol::Both,
        })
    }
}

impl TryFrom<common::FirewallRule> for firewall::Rule {
    type Error = eyre::Error;
    fn try_from(rule: common::FirewallRule) -> Result<Self, Self::Error> {
        let action = rule.action().try_into()?;
        let direction = rule.direction().try_into()?;
        let protocol = Some(rule.protocol().try_into()?);
        Ok(Self {
            name: rule.name,
            action,
            direction,
            protocol,
            ips: rule.ips,
            ports: rule.ports.into_iter().map(|p| p as u16).collect(),
        })
    }
}

impl From<pb::compression::Compression> for Compression {
    fn from(value: pb::compression::Compression) -> Self {
        match value {
            pb::compression::Compression::Zstd(level) => Compression::ZSTD(level),
        }
    }
}

impl TryFrom<pb::checksum::Checksum> for Checksum {
    type Error = eyre::Error;
    fn try_from(value: pb::checksum::Checksum) -> Result<Self, Self::Error> {
        Ok(match value {
            pb::checksum::Checksum::Sha1(bytes) => Checksum::Sha1(try_into_array(bytes)?),
            pb::checksum::Checksum::Sha256(bytes) => Checksum::Sha256(try_into_array(bytes)?),
            pb::checksum::Checksum::Blake3(bytes) => Checksum::Blake3(try_into_array(bytes)?),
        })
    }
}

impl TryFrom<pb::DownloadManifest> for DownloadManifest {
    type Error = eyre::Error;
    fn try_from(manifest: pb::DownloadManifest) -> Result<Self, Self::Error> {
        let compression = if let Some(pb::Compression {
            compression: Some(compression),
        }) = manifest.compression
        {
            Some(compression.into())
        } else {
            None
        };
        let chunks = manifest
            .chunks
            .into_iter()
            .map(|value| {
                let checksum = if let Some(pb::Checksum {
                    checksum: Some(checksum),
                }) = value.checksum
                {
                    checksum.try_into()?
                } else {
                    bail!("Missing checksum")
                };
                let destinations = value
                    .destinations
                    .into_iter()
                    .map(|value| {
                        Ok(FileLocation {
                            path: PathBuf::from_str(&value.path)?,
                            pos: value.position_bytes,
                            size: value.size_bytes,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Chunk {
                    key: value.key,
                    url: value.url.map(|url| Url::parse(&url)).transpose()?,
                    checksum,
                    size: value.size,
                    destinations,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            total_size: manifest.total_size,
            compression,
            chunks,
        })
    }
}

impl From<Compression> for pb::Compression {
    fn from(value: Compression) -> Self {
        match value {
            Compression::ZSTD(level) => pb::Compression {
                compression: Some(pb::compression::Compression::Zstd(level)),
            },
        }
    }
}

impl From<Checksum> for pb::Checksum {
    fn from(value: Checksum) -> Self {
        match value {
            Checksum::Sha1(bytes) => pb::Checksum {
                checksum: Some(pb::checksum::Checksum::Sha1(bytes.to_vec())),
            },
            Checksum::Sha256(bytes) => pb::Checksum {
                checksum: Some(pb::checksum::Checksum::Sha256(bytes.to_vec())),
            },
            Checksum::Blake3(bytes) => pb::Checksum {
                checksum: Some(pb::checksum::Checksum::Blake3(bytes.to_vec())),
            },
        }
    }
}

impl From<DownloadManifest> for pb::DownloadManifest {
    fn from(manifest: DownloadManifest) -> Self {
        let compression = manifest.compression.map(|v| v.into());
        let chunks = manifest
            .chunks
            .into_iter()
            .map(|value| {
                let checksum = value.checksum.into();
                let destinations = value
                    .destinations
                    .into_iter()
                    .map(|value| pb::ChunkTarget {
                        path: value.path.to_string_lossy().to_string(),
                        position_bytes: value.pos,
                        size_bytes: value.size,
                    })
                    .collect();
                pb::ArchiveChunk {
                    key: value.key,
                    url: value.url.map(|url| url.to_string()),
                    checksum: Some(checksum),
                    size: value.size,
                    destinations,
                }
            })
            .collect();
        Self {
            total_size: manifest.total_size,
            compression,
            chunks,
        }
    }
}

impl TryFrom<pb::UploadSlot> for Slot {
    type Error = eyre::Report;
    fn try_from(value: pb::UploadSlot) -> Result<Self> {
        Ok(Self {
            key: value.key,
            url: Url::parse(&value.url)?,
        })
    }
}

impl TryFrom<pb::UploadManifest> for UploadManifest {
    type Error = eyre::Report;
    fn try_from(manifest: pb::UploadManifest) -> Result<Self> {
        Ok(Self {
            slots: manifest
                .slots
                .into_iter()
                .map(|slot| slot.try_into())
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

fn try_into_array<const S: usize>(vec: Vec<u8>) -> Result<[u8; S]> {
    vec.try_into().map_err(|vec: Vec<u8>| {
        anyhow!(
            "expected {} bytes checksum, but {} bytes found",
            S,
            vec.len()
        )
    })
}
