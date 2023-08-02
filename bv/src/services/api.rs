use crate::{
    config::SharedConfig,
    get_bv_status,
    node_data::NodeImage,
    nodes,
    nodes::Nodes,
    pal::Pal,
    services::api::pb::{Action, ChecksumType, Direction, Protocol, Rule},
    ServiceStatus,
};
use anyhow::{anyhow, bail, Context, Result};
use babel_api::{
    engine::{Checksum, Chunk, Compression, DownloadManifest, FileLocation},
    metadata::firewall,
};
use base64::Engine;
use bv_utils::with_retry;
use metrics::{register_counter, Counter};
use pb::{
    command_service_client::CommandServiceClient, discovery_service_client::DiscoveryServiceClient,
    host_service_client::HostServiceClient, manifest_service_client::ManifestServiceClient,
    metrics_service_client, node_command::Command, node_service_client,
};
use std::{
    fmt::Debug,
    path::PathBuf,
    {str::FromStr, sync::Arc},
};
use tokio::time::Instant;
use tonic::{
    codegen::InterceptedService, service::Interceptor, transport::Channel, transport::Endpoint,
    Request, Status,
};
use tracing::{error, info, instrument};
use uuid::Uuid;

use self::pb::{auth_service_client, AuthServiceRefreshResponse};

#[allow(clippy::large_enum_variant)]
pub mod pb {
    tonic::include_proto!("blockjoy.v1");
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
}

pub struct AuthToken(pub String);

impl Interceptor for AuthToken {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let token = &self.0;
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        Ok(request)
    }
}

impl AuthToken {
    pub fn expired(token: &str) -> Result<bool, Status> {
        let margin = chrono::Duration::minutes(1);
        Self::expiration(token).map(|exp| exp < chrono::Utc::now() - margin)
    }

    fn expiration(token: &str) -> Result<chrono::DateTime<chrono::Utc>, Status> {
        use base64::engine::general_purpose::STANDARD_NO_PAD;
        use chrono::TimeZone;

        #[derive(serde::Deserialize)]
        struct Field {
            exp: i64,
        }

        let unauth = |s| move || Status::unauthenticated(s);
        // Take the middle section of the jwt, which has the payload.
        let middle = token
            .split('.')
            .nth(1)
            .ok_or_else(unauth("Can't parse token"))?;
        // Base64 decode the payload.
        let decoded = STANDARD_NO_PAD
            .decode(middle)
            .ok()
            .ok_or_else(unauth("Token is not base64"))?;
        // Json-parse the payload, with only the `exp` field being of interest.
        let parsed: Field = serde_json::from_slice(&decoded)
            .ok()
            .ok_or_else(unauth("Token is not JSON with exp field"))?;
        // Now interpret this timestamp as an utc time.
        match chrono::Utc.timestamp_opt(parsed.exp, 0) {
            chrono::LocalResult::None => Err(unauth("Invalid timestamp")()),
            chrono::LocalResult::Single(expiration) => Ok(expiration),
            chrono::LocalResult::Ambiguous(expiration, _) => Ok(expiration),
        }
    }
}

pub type AuthenticatedService = InterceptedService<Channel, AuthToken>;

pub type MetricsClient = metrics_service_client::MetricsServiceClient<AuthenticatedService>;

impl MetricsClient {
    pub fn with_auth(channel: Channel, token: AuthToken) -> Self {
        metrics_service_client::MetricsServiceClient::with_interceptor(channel, token)
    }
}

pub struct AuthClient {
    client: auth_service_client::AuthServiceClient<Channel>,
}

impl AuthClient {
    pub async fn connect(url: &str) -> Result<Self> {
        let endpoint = Endpoint::from_str(url)?;
        let client = auth_service_client::AuthServiceClient::connect(endpoint).await?;
        Ok(Self { client })
    }

    pub async fn refresh(
        &mut self,
        req: pb::AuthServiceRefreshRequest,
    ) -> Result<AuthServiceRefreshResponse> {
        let resp = self.client.refresh(req).await?;
        Ok(resp.into_inner())
    }
}

pub struct CommandsService {
    client: CommandServiceClient<AuthenticatedService>,
}

impl CommandsService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        let url = config.read().await.blockjoy_api_url;
        let endpoint = Endpoint::from_str(&url)?;
        let endpoint = Endpoint::connect(&endpoint)
            .await
            .with_context(|| format!("Failed to connect to commands service at {url}"))?;
        let client = CommandServiceClient::with_interceptor(endpoint, config.token().await?);

        Ok(Self { client })
    }

    pub async fn get_and_process_pending_commands<P: Pal + Debug>(
        &mut self,
        host_id: &str,
        nodes: Arc<Nodes<P>>,
    ) -> Result<()> {
        let commands = self
            .get_pending_commands(host_id)
            .await
            .with_context(|| "cannot get pending commands")?;
        self.process_commands(commands, nodes)
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
        nodes: Arc<Nodes<P>>,
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
                Some(pb::command::Command::Host(_)) => {
                    let msg = "Command type `Host` not supported".to_string();
                    error!("Error processing command: {msg}");
                    let command_id = command.id;
                    self.send_command_ack(command_id.clone()).await?;
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
        let req = pb::CommandServiceUpdateRequest {
            id: command_id,
            response,
            exit_code,
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
                self.send_command_update(
                    command_id,
                    Some(STATUS_ERROR),
                    Some("service not ready, try again later".to_string()),
                )
                .await
            }
            ServiceStatus::Updating => {
                self.send_command_update(
                    command_id,
                    Some(STATUS_ERROR),
                    Some("pending update, try again later".to_string()),
                )
                .await
            }
            ServiceStatus::Broken => {
                self.send_command_update(
                    command_id,
                    Some(STATUS_ERROR),
                    Some("service is broken, call support".to_string()),
                )
                .await
            }
            ServiceStatus::Ok => Ok(()),
        }
    }
}

async fn process_node_command<P: Pal + Debug>(
    nodes: Arc<Nodes<P>>,
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
                    .chain([("network".to_string(), args.network.clone())])
                    .collect();
                let rules = args
                    .rules
                    .into_iter()
                    .map(|rule| rule.try_into())
                    .collect::<Result<Vec<_>>>()?;
                nodes
                    .create(
                        node_id,
                        nodes::NodeConfig {
                            name: args.name,
                            image,
                            ip: args.ip,
                            gateway: args.gateway,
                            rules,
                            properties,
                            network: args.network,
                        },
                    )
                    .await?;
                API_CREATE_COUNTER.increment(1);
                API_CREATE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Delete(_) => {
                nodes.delete(node_id).await?;
                API_DELETE_COUNTER.increment(1);
                API_DELETE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Start(_) => {
                nodes.start(node_id).await?;
                API_START_COUNTER.increment(1);
                API_START_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Stop(_) => {
                nodes.stop(node_id, false).await?;
                API_STOP_COUNTER.increment(1);
                API_STOP_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Restart(_) => {
                nodes.stop(node_id, false).await?;
                nodes.start(node_id).await?;
                API_RESTART_COUNTER.increment(1);
                API_RESTART_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Upgrade(args) => {
                let image: NodeImage = args
                    .image
                    .ok_or_else(|| anyhow!("Image not provided"))?
                    .into();
                nodes.upgrade(node_id, image).await?;
                API_UPGRADE_COUNTER.increment(1);
                API_UPGRADE_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            }
            Command::Update(pb::NodeUpdate { rules, .. }) => {
                nodes
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
        None => bail!("Node command is `None`"),
    };

    Ok(())
}

pub struct NodesService {
    client: node_service_client::NodeServiceClient<AuthenticatedService>,
}

impl NodesService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        let url = config.read().await.blockjoy_api_url;
        let endpoint = Endpoint::from_str(&url)?;
        let endpoint = Endpoint::connect(&endpoint)
            .await
            .with_context(|| format!("Failed to connect to node service at {url}"))?;
        let client = node_service_client::NodeServiceClient::with_interceptor(
            endpoint,
            config.token().await?,
        );

        Ok(Self { client })
    }

    #[instrument(skip(self))]
    pub async fn send_node_status_update(
        &mut self,
        update: pb::NodeServiceUpdateStatusRequest,
    ) -> Result<()> {
        self.client.update_status(update).await?;
        Ok(())
    }
}

pub struct DiscoveryService {
    client: DiscoveryServiceClient<AuthenticatedService>,
}

impl DiscoveryService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        let url = &config.read().await.blockjoy_api_url;
        let endpoint = Endpoint::from_str(url)?;
        let channel = Endpoint::connect(&endpoint)
            .await
            .with_context(|| format!("Failed to connect to discovery service at {url}"))?;
        let client = DiscoveryServiceClient::with_interceptor(channel, config.token().await?);

        Ok(Self { client })
    }

    #[instrument(skip(self))]
    pub async fn get_services(&mut self) -> Result<pb::DiscoveryServiceServicesResponse> {
        let request = pb::DiscoveryServiceServicesRequest {};
        Ok(self.client.services(request).await?.into_inner())
    }
}

pub struct HostsService {
    client: HostServiceClient<AuthenticatedService>,
}

impl HostsService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        let url = &config.read().await.blockjoy_api_url;
        let endpoint = Endpoint::from_str(url)?;
        let channel = Endpoint::connect(&endpoint)
            .await
            .with_context(|| format!("Failed to connect to discovery service at {url}"))?;
        let client = HostServiceClient::with_interceptor(channel, config.token().await?);
        Ok(Self { client })
    }

    #[instrument(skip(self))]
    pub async fn update(
        &mut self,
        request: pb::HostServiceUpdateRequest,
    ) -> Result<pb::HostServiceUpdateResponse> {
        Ok(self.client.update(request).await?.into_inner())
    }
}

pub struct ManifestService {
    client: ManifestServiceClient<AuthenticatedService>,
}

impl ManifestService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        let url = &config.read().await.blockjoy_api_url;
        let endpoint = Endpoint::from_str(url)?;
        let channel = Endpoint::connect(&endpoint)
            .await
            .with_context(|| format!("Failed to connect to manifest service at {url}"))?;
        let client = ManifestServiceClient::with_interceptor(channel, config.token().await?);
        Ok(Self { client })
    }

    #[instrument(skip(self))]
    pub async fn retrieve_manifest(
        &mut self,
        image: &NodeImage,
        network: &str,
    ) -> Result<DownloadManifest> {
        let manifest = self
            .client
            .retrieve_download_manifest(pb::ManifestServiceRetrieveDownloadManifestRequest {
                id: Some(image.clone().into()),
                network: network.to_owned(),
            })
            .await?
            .into_inner();
        manifest
            .manifest
            .ok_or_else(|| anyhow!("manifest not found for {:?}-{}", image, network))?
            .try_into()
    }
}

impl From<pb::ContainerImage> for NodeImage {
    fn from(image: pb::ContainerImage) -> Self {
        Self {
            protocol: image.protocol.to_lowercase(),
            node_type: image.node_type().to_string(),
            node_version: image.node_version.to_lowercase(),
        }
    }
}

impl std::fmt::Display for pb::NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.as_str_name();
        let s = s.strip_prefix("NODE_TYPE_").unwrap_or(s).to_lowercase();
        write!(f, "{s}")
    }
}

impl TryFrom<Action> for firewall::Action {
    type Error = anyhow::Error;
    fn try_from(value: Action) -> Result<Self, Self::Error> {
        Ok(match value {
            Action::Unspecified => {
                bail!("Invalid Action")
            }
            Action::Allow => firewall::Action::Allow,
            Action::Deny => firewall::Action::Deny,
            Action::Reject => firewall::Action::Reject,
        })
    }
}

fn try_action(value: i32) -> Result<firewall::Action> {
    Action::from_i32(value)
        .unwrap_or(Action::Unspecified)
        .try_into()
}

impl TryFrom<Direction> for firewall::Direction {
    type Error = anyhow::Error;
    fn try_from(value: Direction) -> Result<Self, Self::Error> {
        Ok(match value {
            Direction::Unspecified => {
                bail!("Invalid Direction")
            }
            Direction::In => firewall::Direction::In,
            Direction::Out => firewall::Direction::Out,
        })
    }
}

impl TryFrom<Protocol> for firewall::Protocol {
    type Error = anyhow::Error;
    fn try_from(value: Protocol) -> Result<Self, Self::Error> {
        Ok(match value {
            Protocol::Unspecified => bail!("Invalid Protocol"),
            Protocol::Tcp => firewall::Protocol::Tcp,
            Protocol::Udp => firewall::Protocol::Udp,
            Protocol::Both => firewall::Protocol::Both,
        })
    }
}

impl TryFrom<Rule> for firewall::Rule {
    type Error = anyhow::Error;
    fn try_from(rule: Rule) -> Result<Self, Self::Error> {
        let direction = rule.direction().try_into()?;
        let protocol = Some(rule.protocol().try_into()?);
        Ok(Self {
            name: rule.name,
            action: try_action(rule.action)?,
            direction,
            protocol,
            ips: rule.ips,
            ports: rule.ports.into_iter().map(|p| p as u16).collect(),
        })
    }
}

impl From<NodeImage> for pb::ConfigIdentifier {
    fn from(image: NodeImage) -> Self {
        Self {
            protocol: image.protocol,
            node_type: image.node_type,
            node_version: image.node_version,
        }
    }
}

impl TryFrom<pb::Compression> for Compression {
    type Error = anyhow::Error;
    fn try_from(_value: pb::Compression) -> Result<Self, Self::Error> {
        bail!("Invalid compression type");
    }
}

impl TryFrom<pb::DownloadManifest> for DownloadManifest {
    type Error = anyhow::Error;
    fn try_from(manifest: pb::DownloadManifest) -> Result<Self, Self::Error> {
        let compression = if manifest.compression.is_some() {
            Some(manifest.compression().try_into()?)
        } else {
            None
        };
        let chunks = manifest
            .chunks
            .into_iter()
            .map(|value| {
                let checksum = match value.checksum_type() {
                    ChecksumType::Unspecified => {
                        bail!("Invalid checksum type")
                    }
                    ChecksumType::Sha1 => Checksum::Sha1(try_into_array(value.checksum)?),
                    ChecksumType::Sha256 => Checksum::Sha256(try_into_array(value.checksum)?),
                    ChecksumType::Blake3 => Checksum::Blake3(try_into_array(value.checksum)?),
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
                    url: value.url,
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

fn try_into_array<const S: usize>(vec: Vec<u8>) -> Result<[u8; S], anyhow::Error> {
    vec.try_into().map_err(|vec: Vec<u8>| {
        anyhow!(
            "expected {} bytes checksum, but {} bytes found",
            S,
            vec.len()
        )
    })
}
