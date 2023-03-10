use crate::services::api::pb::{Direction, NodeFirewallUpdate, Policy, Protocol};
use crate::{
    config::Config,
    node_data::NodeImage,
    nodes::Nodes,
    pal::Pal,
    server::bv_pb,
    services::api::pb::ServicesResponse,
    {get_bv_status, with_retry},
};
use anyhow::{anyhow, bail, Context, Result};
use babel_api::config::firewall;
use base64::Engine;
use metrics::{register_counter, Counter};
use pb::{
    commands_client::CommandsClient, discovery_client::DiscoveryClient,
    metrics_service_client::MetricsServiceClient, node_command::Command, nodes_client::NodesClient,
};
use std::{
    fmt::Debug,
    {str::FromStr, sync::Arc},
};
use tokio::sync::RwLock;
use tokio::time::Instant;
use tonic::{
    codegen::InterceptedService, service::Interceptor, transport::Channel, Request, Status,
};
use tracing::{error, info, instrument};
use uuid::Uuid;

#[allow(clippy::large_enum_variant)]
pub mod pb {
    tonic::include_proto!("blockjoy.api.v1");
}

const STATUS_OK: i32 = 0;
const STATUS_ERROR: i32 = 1;

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
    pub static ref API_FIREWALL_UPDATE_COUNTER: Counter = register_counter!("api.commands.firewall_update.calls");
    pub static ref API_FIREWALL_UPDATE_TIME_MS_COUNTER: Counter = register_counter!("api.commands.firewall_update.ms");
}

#[derive(Clone)]
pub struct AuthToken(pub String);

pub type MetricsClient = MetricsServiceClient<InterceptedService<Channel, AuthToken>>;

impl Interceptor for AuthToken {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let mut request = request;
        let val = format!(
            "Bearer {}",
            base64::engine::general_purpose::STANDARD.encode(self.0.clone())
        );
        request
            .metadata_mut()
            .insert("authorization", val.parse().unwrap());
        Ok(request)
    }
}

impl MetricsClient {
    pub fn with_auth(channel: Channel, token: AuthToken) -> Self {
        MetricsServiceClient::with_interceptor(channel, token)
    }
}

pub struct CommandsService {
    token: String,
    client: CommandsClient<Channel>,
}

impl CommandsService {
    pub async fn connect(config: Config) -> Result<Self> {
        let url = config.blockjoy_api_url;
        let client = CommandsClient::connect(url.clone())
            .await
            .context(format!("Failed to connect to commands service at {url}"))?;

        Ok(Self {
            token: config.token,
            client,
        })
    }

    pub async fn get_and_process_pending_commands<P: Pal + Debug>(
        &mut self,
        host_id: &str,
        nodes: Arc<RwLock<Nodes<P>>>,
    ) -> Result<()> {
        info!("Get and process pending commands");

        let commands = self.get_pending_commands(host_id).await?;
        self.process_commands(commands, nodes).await
    }

    pub async fn get_pending_commands(&mut self, host_id: &str) -> Result<Vec<pb::Command>> {
        info!("Get pending commands");

        let req = pb::PendingCommandsRequest {
            host_id: host_id.to_string(),
            filter_type: None,
        };
        let resp =
            with_retry!(self.client.pending(with_auth(req.clone(), &self.token)))?.into_inner();

        Ok(resp.commands)
    }

    pub async fn process_commands<P: Pal + Debug>(
        &mut self,
        commands: Vec<pb::Command>,
        nodes: Arc<RwLock<Nodes<P>>>,
    ) -> Result<()> {
        info!("Processing commands");

        for command in commands {
            info!("Processing command: {command:?}");

            match command.r#type {
                Some(pb::command::Type::Node(node_command)) => {
                    let command_id = node_command.api_command_id.clone();
                    // check for bv health status
                    let service_status = get_bv_status().await;
                    if service_status != bv_pb::ServiceStatus::Ok {
                        self.send_service_status_update(command_id.clone(), service_status)
                            .await?;
                    } else {
                        // process the command
                        match process_node_command(nodes.clone(), node_command).await {
                            Err(error) => {
                                error!("Error processing command: {error}");
                                self.send_command_update(
                                    command_id,
                                    Some(STATUS_ERROR),
                                    Some(error.to_string()),
                                )
                                .await?;
                            }
                            Ok(()) => {
                                self.send_command_update(command_id, Some(STATUS_OK), None)
                                    .await?;
                            }
                        }
                    }
                }
                Some(pb::command::Type::Host(host_command)) => {
                    let msg = "Command type `Host` not supported".to_string();
                    error!("Error processing command: {msg}");
                    let command_id = host_command.api_command_id;
                    self.send_command_update(command_id, Some(STATUS_ERROR), Some(msg))
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
        exit_code: Option<i32>,
        response: Option<String>,
    ) -> Result<()> {
        let req = pb::CommandInfo {
            id: command_id,
            response,
            exit_code,
        };
        with_retry!(self.client.update(with_auth(req.clone(), &self.token)))?;
        Ok(())
    }

    async fn send_service_status_update(
        &mut self,
        command_id: String,
        status: bv_pb::ServiceStatus,
    ) -> Result<()> {
        match status {
            bv_pb::ServiceStatus::UndefinedServiceStatus => {
                self.send_command_update(
                    command_id,
                    Some(STATUS_ERROR),
                    Some("service not ready, try again later".to_string()),
                )
                .await
            }
            bv_pb::ServiceStatus::Updating => {
                self.send_command_update(
                    command_id,
                    Some(STATUS_ERROR),
                    Some("pending update, try again later".to_string()),
                )
                .await
            }
            bv_pb::ServiceStatus::Broken => {
                self.send_command_update(
                    command_id,
                    Some(STATUS_ERROR),
                    Some("service is broken, call support".to_string()),
                )
                .await
            }
            bv_pb::ServiceStatus::Ok => Ok(()),
        }
    }
}

async fn process_node_command<P: Pal + Debug>(
    nodes: Arc<RwLock<Nodes<P>>>,
    node_command: pb::NodeCommand,
) -> Result<()> {
    let node_id = Uuid::from_str(&node_command.node_id)?;
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
                    .collect();
                nodes
                    .write()
                    .await
                    .create(node_id, args.name, image, args.ip, args.gateway, properties)
                    .await?;
                API_CREATE_COUNTER.increment(1);
                API_CREATE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Delete(_) => {
                nodes.write().await.delete(node_id).await?;
                API_DELETE_COUNTER.increment(1);
                API_DELETE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Start(_) => {
                nodes.write().await.start(node_id).await?;
                API_START_COUNTER.increment(1);
                API_START_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Stop(_) => {
                nodes.write().await.stop(node_id).await?;
                API_STOP_COUNTER.increment(1);
                API_STOP_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Restart(_) => {
                nodes.write().await.stop(node_id).await?;
                nodes.write().await.start(node_id).await?;
                API_RESTART_COUNTER.increment(1);
                API_RESTART_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Upgrade(args) => {
                let image: NodeImage = args
                    .image
                    .ok_or_else(|| anyhow!("Image not provided"))?
                    .into();
                nodes.write().await.upgrade(node_id, image).await?;
                API_UPGRADE_COUNTER.increment(1);
                API_UPGRADE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Update(pb::NodeInfoUpdate {
                name,
                self_update,
                properties,
            }) => {
                nodes
                    .write()
                    .await
                    .update(node_id, name, self_update, properties)
                    .await?;
                API_UPDATE_COUNTER.increment(1);
                API_UPDATE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::InfoGet(_) => unimplemented!(),
            Command::Generic(_) => unimplemented!(),
            Command::FirewallUpdate(update) => {
                nodes
                    .write()
                    .await
                    .firewall_update(node_id, update.try_into()?)
                    .await?;
                API_FIREWALL_UPDATE_COUNTER.increment(1);
                API_FIREWALL_UPDATE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
        },
        None => bail!("Node command is `None`"),
    };

    Ok(())
}

pub struct NodesService {
    token: String,
    client: NodesClient<Channel>,
}

impl NodesService {
    pub async fn connect(config: Config) -> Result<Self> {
        let url = config.blockjoy_api_url;
        let client = NodesClient::connect(url.clone())
            .await
            .with_context(|| format!("Failed to connect to nodes service at {url}"))?;

        Ok(Self {
            token: config.token,
            client,
        })
    }

    #[instrument(skip(self))]
    pub async fn send_node_update(&mut self, update: pb::NodeInfo) -> Result<()> {
        let req = pb::NodeInfoUpdateRequest {
            request_id: Some(Uuid::new_v4().to_string()),
            info: Some(update),
        };
        self.client
            .info_update(with_auth(req.clone(), &self.token))
            .await?;
        Ok(())
    }
}

pub struct DiscoveryService {
    token: String,
    client: DiscoveryClient<Channel>,
}

impl DiscoveryService {
    pub async fn connect(config: Config) -> Result<Self> {
        let url = config.blockjoy_api_url;
        let client = DiscoveryClient::connect(url.clone())
            .await
            .with_context(|| format!("Failed to connect to discovery service at {url}"))?;

        Ok(Self {
            token: config.token,
            client,
        })
    }

    #[instrument(skip(self))]
    pub async fn get_services(&mut self) -> Result<ServicesResponse> {
        Ok(self
            .client
            .services(with_auth((), &self.token))
            .await?
            .into_inner())
    }
}

pub fn with_auth<T>(inner: T, auth_token: &str) -> Request<T> {
    let mut request = Request::new(inner);
    request.metadata_mut().insert(
        "authorization",
        format!(
            "Bearer {}",
            base64::engine::general_purpose::STANDARD.encode(auth_token)
        )
        .parse()
        .unwrap(),
    );
    request
}

impl From<pb::ContainerImage> for NodeImage {
    fn from(image: pb::ContainerImage) -> Self {
        Self {
            protocol: image.protocol.to_lowercase(),
            node_type: image.node_type.to_lowercase(),
            node_version: image.node_version.to_lowercase(),
        }
    }
}

impl TryFrom<Policy> for firewall::Policy {
    type Error = anyhow::Error;
    fn try_from(value: Policy) -> std::result::Result<Self, Self::Error> {
        Ok(match value {
            Policy::UndefinedPolicy => {
                bail!("Invalid Policy")
            }
            Policy::Allow => firewall::Policy::Allow,
            Policy::Deny => firewall::Policy::Deny,
            Policy::Reject => firewall::Policy::Reject,
        })
    }
}

fn try_policy(value: i32) -> Result<firewall::Policy> {
    Policy::from_i32(value)
        .unwrap_or(Policy::UndefinedPolicy)
        .try_into()
}

impl TryFrom<Direction> for firewall::Direction {
    type Error = anyhow::Error;
    fn try_from(value: Direction) -> std::result::Result<Self, Self::Error> {
        Ok(match value {
            Direction::UndefinedDirection => {
                bail!("Invalid Direction")
            }
            Direction::In => firewall::Direction::In,
            Direction::Out => firewall::Direction::Out,
        })
    }
}

impl TryFrom<Protocol> for firewall::Protocol {
    type Error = anyhow::Error;
    fn try_from(value: Protocol) -> std::result::Result<Self, Self::Error> {
        Ok(match value {
            Protocol::Both => firewall::Protocol::Both,
            Protocol::Tcp => firewall::Protocol::Tcp,
            Protocol::Udp => firewall::Protocol::Udp,
        })
    }
}

impl TryFrom<NodeFirewallUpdate> for firewall::Config {
    type Error = anyhow::Error;
    fn try_from(update: NodeFirewallUpdate) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            enabled: update.enabled,
            default_in: try_policy(update.default_in)?,
            default_out: try_policy(update.default_out)?,
            rules: update
                .rules
                .into_iter()
                .map(|rule| {
                    Ok(firewall::Rule {
                        name: rule.name,
                        policy: try_policy(rule.policy)?,
                        direction: Direction::from_i32(rule.direction)
                            .unwrap_or(Direction::UndefinedDirection)
                            .try_into()?,
                        protocol: Some(
                            Protocol::from_i32(rule.policy)
                                .unwrap_or(Protocol::Both)
                                .try_into()?,
                        ),
                        ips: rule.ips,
                        ports: rule.ports.into_iter().map(|p| p as u16).collect(),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        })
    }
}
