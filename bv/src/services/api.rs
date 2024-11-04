use crate::{
    api_with_retry,
    bv_config::SharedConfig,
    command_failed, commands,
    commands::Error,
    firewall, get_bv_status,
    node_state::{self, NodeState, ProtocolImageKey, VmStatus},
    nodes_manager::NodesManager,
    pal::Pal,
    services::{ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService},
    ServiceStatus,
};
use babel_api::utils::{BabelConfig, RamdiskConfiguration};
use eyre::{anyhow, bail, Context, Result};
use metrics::{register_counter, Counter};
use pb::{
    archive_service_client, command_service_client, discovery_service_client, host_service_client,
    image_service_client, metrics_service_client, node_command::Command, node_service_client,
    protocol_service_client,
};
use std::{
    collections::HashMap,
    fmt::Debug,
    {str::FromStr, sync::Arc},
};
use tokio::time::Instant;
use tonic::transport::Channel;
use tracing::{error, info, instrument, warn};
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

pub type ProtocolServiceClient =
    protocol_service_client::ProtocolServiceClient<AuthenticatedService>;
pub type ArchiveServiceClient = archive_service_client::ArchiveServiceClient<AuthenticatedService>;
pub type DiscoveryServiceClient =
    discovery_service_client::DiscoveryServiceClient<AuthenticatedService>;
pub type HostsServiceClient = host_service_client::HostServiceClient<AuthenticatedService>;
pub type NodesServiceClient = node_service_client::NodeServiceClient<AuthenticatedService>;
pub type ImageServiceClient = image_service_client::ImageServiceClient<AuthenticatedService>;
pub type CommandServiceClient = command_service_client::CommandServiceClient<AuthenticatedService>;
pub type MetricsServiceClient = metrics_service_client::MetricsServiceClient<AuthenticatedService>;

async fn connect_command_service(
    config: &SharedConfig,
) -> Result<
    ApiClient<
        CommandServiceClient,
        impl ApiServiceConnector,
        impl Fn(Channel, ApiInterceptor) -> CommandServiceClient + Clone,
    >,
    tonic::Status,
> {
    let client = ApiClient::build_with_default_connector(
        config,
        command_service_client::CommandServiceClient::with_interceptor,
    )
    .await;
    if let Err(err) = &client {
        warn!("error connecting to api while processing commands: {err:#}");
    }
    client
}

pub async fn get_and_process_pending_commands<P>(
    config: &SharedConfig,
    nodes_manager: Arc<NodesManager<P>>,
) where
    P: Pal + Send + Sync + Debug + 'static,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    let service = CommandsService { config };
    if let Some(commands) = service.get_pending_commands().await {
        if let Err(err) = service
            .process_commands(commands, nodes_manager.clone())
            .await
        {
            error!("error processing commands: {err:#}");
        }
    }
}

struct CommandsService<'a> {
    config: &'a SharedConfig,
}

impl<'a> CommandsService<'a> {
    async fn get_pending_commands(&self) -> Option<Vec<pb::Command>> {
        info!("Get pending commands");

        let req = pb::CommandServicePendingRequest {
            host_id: self.config.read().await.id.to_string(),
            filter_type: None,
        };
        let mut client = connect_command_service(self.config).await.ok()?;
        match api_with_retry!(client, client.pending(req.clone())) {
            Ok(resp) => {
                let commands = resp.into_inner().commands;
                for command in &commands {
                    let command_id = &command.command_id;
                    let req = pb::CommandServiceAckRequest {
                        command_id: command_id.clone(),
                    };
                    if let Err(err) = api_with_retry!(client, client.ack(req.clone())) {
                        warn!("failed to send ACK for command {command_id}: {err:#}");
                    }
                }
                Some(commands)
            }
            Err(err) => {
                warn!("cannot get pending commands: {err:#}");
                None
            }
        }
    }

    async fn process_commands<P>(
        &self,
        commands: Vec<pb::Command>,
        nodes_manager: Arc<NodesManager<P>>,
    ) -> Result<()>
    where
        P: Pal + Send + Sync + Debug + 'static,
        P::NodeConnection: Send + Sync,
        P::ApiServiceConnector: Send + Sync,
        P::VirtualMachine: Send + Sync,
        P::RecoveryBackoff: Send + Sync + 'static,
    {
        info!("Processing {} commands", commands.len());
        for command in commands {
            info!("Processing command: {command:?}");
            let command_id = &command.command_id;

            // check for bv health status
            let service_status = get_bv_status().await;
            if service_status == ServiceStatus::Ok {
                // process the command
                let command_result = match command.command {
                    Some(pb::command::Command::Node(node_command)) => {
                        process_node_command(nodes_manager.clone(), node_command).await
                    }
                    Some(pb::command::Command::Host(host_command)) => {
                        process_host_command(host_command)
                    }
                    None => {
                        bail!("command '{command_id}' type is `None`");
                    }
                };
                self.send_command_update(command_id, &command_result)
                    .await?;
                command_result.with_context(|| format!("command '{command_id}' failed"))?;
            } else {
                self.send_service_status_update(command_id, service_status)
                    .await?;
                bail!(
                    "can't process command '{command_id}' while BV status is '{service_status:?}'"
                );
            }
        }
        Ok(())
    }

    /// Informing API that we have finished with the command.
    #[instrument(skip(self))]
    async fn send_command_update(
        &self,
        command_id: &str,
        command_result: &commands::Result<()>,
    ) -> Result<()> {
        let req = match command_result {
            Ok(()) => pb::CommandServiceUpdateRequest {
                command_id: command_id.to_string(),
                exit_code: Some(pb::CommandExitCode::Ok.into()),
                exit_message: None,
                retry_hint_seconds: None,
            },
            Err(err) => {
                let mut req = pb::CommandServiceUpdateRequest {
                    command_id: command_id.to_string(),
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
        let mut client = connect_command_service(self.config).await?;
        api_with_retry!(client, client.update(req.clone()))
            .with_context(|| format!("failed to update command '{command_id}' status"))?;
        Ok(())
    }

    async fn send_service_status_update(
        &self,
        command_id: &str,
        status: ServiceStatus,
    ) -> Result<()> {
        match status {
            ServiceStatus::Undefined => {
                self.send_command_update(command_id, &Err(commands::Error::ServiceNotReady))
                    .await
            }
            ServiceStatus::Updating => {
                self.send_command_update(command_id, &Err(commands::Error::ServiceNotReady))
                    .await
            }
            ServiceStatus::Broken => {
                self.send_command_update(command_id, &Err(commands::Error::ServiceBroken))
                    .await
            }
            ServiceStatus::Ok => Ok(()),
        }
    }
}

async fn process_node_command<P>(
    nodes_manager: Arc<NodesManager<P>>,
    node_command: pb::NodeCommand,
) -> commands::Result<()>
where
    P: Pal + Send + Sync + Debug + 'static,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    let node_id = Uuid::from_str(&node_command.node_id).map_err(|err| anyhow!(err))?;
    let now = Instant::now();
    match node_command.command {
        Some(cmd) => match cmd {
            Command::Create(args) => {
                let node_state: NodeState = args
                    .node
                    .ok_or_else(|| anyhow!("Missing node details"))?
                    .try_into()?;
                nodes_manager.create(node_state).await?;
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
                nodes_manager.restart(node_id, false).await?;
                API_RESTART_COUNTER.increment(1);
                API_RESTART_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Upgrade(args) => {
                let node_config: NodeState = args
                    .node
                    .ok_or_else(|| anyhow!("Missing node details"))?
                    .try_into()?;
                nodes_manager.upgrade(node_config).await?;
                API_UPGRADE_COUNTER.increment(1);
                API_UPGRADE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Update(pb::NodeUpdate {
                new_display_name,
                new_firewall,
                new_org_id,
                new_org_name,
                new_values,
                ..
            }) => {
                let firewall_config = if let Some(firewall_config) = new_firewall {
                    Some(firewall_config.try_into()?)
                } else {
                    None
                };
                nodes_manager
                    .update(
                        node_id,
                        new_display_name,
                        firewall_config,
                        new_org_id,
                        new_org_name,
                        HashMap::from_iter(
                            new_values.into_iter().map(|prop| (prop.key, prop.value)),
                        ),
                    )
                    .await?;
                API_UPDATE_COUNTER.increment(1);
                API_UPDATE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
        },
        None => command_failed!(Error::Internal(anyhow!("Node command is `None`"))),
    };
    Ok(())
}

fn process_host_command(host_command: pb::HostCommand) -> commands::Result<()> {
    match host_command.command {
        Some(cmd) => match cmd {
            pb::host_command::Command::Pending(_) => {}
            pb::host_command::Command::Start(_)
            | pb::host_command::Command::Stop(_)
            | pb::host_command::Command::Restart(_) => {
                command_failed!(Error::NotSupported)
            }
        },
        None => command_failed!(Error::Internal(anyhow!("Host command is `None`"))),
    };
    Ok(())
}

impl TryFrom<common::FirewallConfig> for firewall::Config {
    type Error = eyre::Error;
    fn try_from(config: common::FirewallConfig) -> Result<Self, Self::Error> {
        let default_in = config.default_in().try_into()?;
        let default_out = config.default_out().try_into()?;

        let rules = config
            .rules
            .into_iter()
            .map(|rule| rule.try_into())
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            default_in,
            default_out,
            rules,
        })
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
            common::FirewallAction::Drop => firewall::Action::Deny,
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
            name: rule.key,
            action,
            direction,
            protocol,
            ips: rule.ips.into_iter().map(|ip| ip.ip).collect(),
            ports: rule.ports.into_iter().map(|p| p.port as u16).collect(),
        })
    }
}

impl From<common::VmConfig> for node_state::VmConfig {
    fn from(value: common::VmConfig) -> Self {
        Self {
            vcpu_count: value.cpu_cores as usize,
            mem_size_mb: value.memory_bytes / 1_000_000,
            disk_size_gb: value.disk_bytes / 1_000_000_000,
            babel_config: BabelConfig {
                ramdisks: value
                    .ramdisks
                    .into_iter()
                    .map(|ramdisk| RamdiskConfiguration {
                        ram_disk_mount_point: ramdisk.mount,
                        ram_disk_size_mb: ramdisk.size_bytes / 1_000_000,
                    })
                    .collect(),
            },
        }
    }
}

impl From<common::ProtocolVersionKey> for ProtocolImageKey {
    fn from(key: common::ProtocolVersionKey) -> Self {
        Self {
            protocol_key: key.protocol_key,
            variant_key: key.variant_key,
        }
    }
}

impl TryFrom<pb::Node> for NodeState {
    type Error = eyre::Error;
    fn try_from(node: pb::Node) -> Result<Self, Self::Error> {
        let config = node.config.ok_or_else(|| anyhow!("Missing node config"))?;
        let image_config = config.image.ok_or_else(|| anyhow!("Missing image"))?;
        let image = node_state::NodeImage {
            id: node.image_id,
            version: node.semantic_version,
            config_id: node.config_id,
            archive_id: image_config.archive_id,
            store_id: image_config.store_id,
            uri: image_config.image_uri,
        };
        let properties = image_config
            .values
            .into_iter()
            .map(|p| (p.key, p.value))
            .collect();
        let firewall = config
            .firewall
            .map(|config| config.try_into())
            .unwrap_or(Ok(Default::default()))?;
        let vm = config.vm.ok_or_else(|| anyhow!("Missing vm config"))?;
        Ok(Self {
            id: Uuid::from_str(&node.node_id).with_context(|| {
                format!(
                    "node_create received invalid node id from API: {}",
                    node.node_id
                )
            })?,
            name: node.node_name,
            image,
            vm_config: vm.into(),
            ip: node
                .ip_address
                .parse()
                .with_context(|| format!("invalid ip `{}`", node.ip_address))?,
            gateway: node
                .ip_gateway
                .parse()
                .with_context(|| format!("invalid gateway `{}`", node.ip_gateway))?,
            firewall,
            properties,
            protocol_id: node.protocol_id,
            image_key: node
                .version_key
                .ok_or_else(|| anyhow!("Missing version_key"))?
                .into(),
            protocol_name: node.protocol_name,
            org_id: node.org_id,
            display_name: node.display_name.unwrap_or_default(),
            org_name: node.org_name,
            dns_name: node.dns_name,
            assigned_cpus: vec![],
            expected_status: VmStatus::Stopped,
            started_at: None,
            initialized: false,
            dev_mode: false,
            restarting: false,
            apptainer_config: None,
        })
    }
}
