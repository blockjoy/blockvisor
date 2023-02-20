use crate::node_data::NodeImage;
use crate::nodes::Nodes;
use crate::pal::Pal;
use crate::server::bv_pb;
use crate::{get_bv_status, with_retry};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use pb::commands_client::CommandsClient;
use pb::metrics_service_client::MetricsServiceClient;
use pb::node_command::Command;
use pb::nodes_client::NodesClient;
use std::fmt::Debug;
use std::{str::FromStr, sync::Arc};
use tokio::sync::RwLock;
use tonic::codegen::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::Channel;
use tonic::{Request, Status};
use tracing::{error, info, instrument};
use uuid::Uuid;

#[allow(clippy::large_enum_variant)]
pub mod pb {
    tonic::include_proto!("blockjoy.api.v1");
}

const STATUS_OK: i32 = 0;
const STATUS_ERROR: i32 = 1;

#[derive(Clone)]
pub struct AuthToken(pub String);

pub type MetricsClient =
    MetricsServiceClient<InterceptedService<tonic::transport::Channel, AuthToken>>;

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
    pub fn with_auth(channel: tonic::transport::Channel, token: AuthToken) -> Self {
        MetricsServiceClient::with_interceptor(channel, token)
    }
}

pub struct CommandsService {
    token: String,
    client: CommandsClient<Channel>,
}

impl CommandsService {
    pub async fn connect(url: &str, token: &str) -> Result<Self> {
        let client = CommandsClient::connect(url.to_string())
            .await
            .context(format!("Failed to connect to commands service at {url}"))?;

        Ok(Self {
            token: token.to_string(),
            client,
        })
    }

    #[instrument(skip_all)]
    pub async fn process_pending_commands<P: Pal + Debug>(
        &mut self,
        nodes: Arc<RwLock<Nodes<P>>>,
    ) -> Result<()> {
        info!("Processing pending commands");

        let req = pb::PendingCommandsRequest {
            host_id: nodes.read().await.api_config.id.clone(),
            filter_type: None,
        };
        let resp =
            with_retry!(self.client.pending(with_auth(req.clone(), &self.token)))?.into_inner();

        for command in resp.commands {
            info!("Received command: {command:?}");

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
            }
            Command::Delete(_) => {
                nodes.write().await.delete(node_id).await?;
            }
            Command::Start(_) => {
                nodes.write().await.start(node_id).await?;
            }
            Command::Stop(_) => {
                nodes.write().await.stop(node_id).await?;
            }
            Command::Restart(_) => {
                nodes.write().await.stop(node_id).await?;
                nodes.write().await.start(node_id).await?;
            }
            Command::Upgrade(args) => {
                let image: NodeImage = args
                    .image
                    .ok_or_else(|| anyhow!("Image not provided"))?
                    .into();
                nodes.write().await.upgrade(node_id, image).await?;
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
            }
            Command::InfoGet(_) => unimplemented!(),
            Command::Generic(_) => unimplemented!(),
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
    pub async fn connect(url: &str, token: &str) -> Result<Self> {
        let client = NodesClient::connect(url.to_string())
            .await
            .with_context(|| format!("Failed to connect to nodes service at {url}"))?;

        Ok(Self {
            token: token.to_string(),
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
