use crate::get_bv_status;
use crate::node_data::{NodeImage, NodeStatus};
use crate::nodes::Nodes;
use crate::server::bv_pb;
use anyhow::{anyhow, bail, Result};
use pb::command_flow_client::CommandFlowClient;
use pb::metrics_service_client::MetricsServiceClient;
use pb::node_command::Command;
use std::{str::FromStr, sync::Arc};
use tokio::sync::{broadcast::Sender, RwLock};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::codegen::InterceptedService;
use tonic::service::Interceptor;
use tonic::{Request, Status};
use tracing::{error, info};
use uuid::Uuid;

pub mod pb {
    tonic::include_proto!("blockjoy.api.v1");
}

const STATUS_OK: i32 = 0;
const STATUS_ERROR: i32 = 1;

#[derive(Clone)]
pub struct AuthToken(pub String);

pub type CommandsClient =
    CommandFlowClient<InterceptedService<tonic::transport::Channel, AuthToken>>;
pub type MetricsClient =
    MetricsServiceClient<InterceptedService<tonic::transport::Channel, AuthToken>>;

impl Interceptor for AuthToken {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let mut request = request;
        let val = format!("Bearer {}", base64::encode(self.0.clone()));
        request
            .metadata_mut()
            .insert("authorization", val.parse().unwrap());
        Ok(request)
    }
}

impl CommandsClient {
    pub fn with_auth(channel: tonic::transport::Channel, token: AuthToken) -> Self {
        CommandFlowClient::with_interceptor(channel, token)
    }
}

impl MetricsClient {
    pub fn with_auth(channel: tonic::transport::Channel, token: AuthToken) -> Self {
        MetricsServiceClient::with_interceptor(channel, token)
    }
}

fn service_status_update(
    status: bv_pb::ServiceStatus,
    command_id: String,
) -> Option<pb::InfoUpdate> {
    match status {
        bv_pb::ServiceStatus::UndefinedServiceStatus => create_info_update(
            command_id,
            STATUS_ERROR,
            Some("service not ready, try again later".to_string()),
        ),
        bv_pb::ServiceStatus::Updating => create_info_update(
            command_id,
            STATUS_ERROR,
            Some("pending update, try again later".to_string()),
        ),
        bv_pb::ServiceStatus::Broken => create_info_update(
            command_id,
            STATUS_ERROR,
            Some("service is broken, call support".to_string()),
        ),
        bv_pb::ServiceStatus::Ok => None,
    }
}

pub async fn process_commands_stream(
    client: &mut CommandsClient,
    nodes: Arc<RwLock<Nodes>>,
    updates_tx: Sender<pb::InfoUpdate>,
) -> Result<()> {
    info!("Processing pending commands");
    let updates_rx = updates_tx.subscribe();

    let updates_stream = BroadcastStream::new(updates_rx).filter_map(|item| item.ok());

    let response = client.commands(updates_stream).await?;
    let mut commands_stream = response.into_inner();

    info!("Getting pending commands from stream...");
    while let Some(received) = commands_stream.next().await {
        info!("Received command: {received:?}");
        let received = received?;

        let update = match received.r#type {
            Some(pb::command::Type::Node(node_command)) => {
                let command_id = node_command.api_command_id.clone();
                let update = service_status_update(get_bv_status().await, command_id.clone());
                if update.is_none() {
                    match process_node_command(nodes.clone(), node_command).await {
                        Err(error) => {
                            error!("Error processing command: {error}");
                            create_info_update(command_id, STATUS_ERROR, Some(error.to_string()))
                        }
                        Ok(()) => create_info_update(command_id, STATUS_OK, None),
                    }
                } else {
                    update
                }
            }
            Some(pb::command::Type::Host(host_command)) => {
                let msg = "Command type `Host` not supported".to_string();
                error!("Error processing command: {msg}");
                let command_id = host_command.api_command_id;
                create_info_update(command_id, STATUS_ERROR, Some(msg))
            }
            None => {
                let msg = "Command type is `None`".to_string();
                error!("Error processing command: {msg}");
                None
            }
        };

        if let Some(update) = update {
            updates_tx.send(update)?;
        }
    }

    Ok(())
}

fn create_info_update(
    command_id: String,
    code: i32,
    msg: Option<String>,
) -> Option<pb::InfoUpdate> {
    Some(pb::InfoUpdate {
        info: Some(pb::info_update::Info::Command(pb::CommandInfo {
            id: command_id,
            response: msg,
            exit_code: Some(code),
        })),
    })
}

async fn process_node_command(
    nodes: Arc<RwLock<Nodes>>,
    node_command: pb::NodeCommand,
) -> Result<()> {
    let node_id = Uuid::from_str(&node_command.id)?;
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
                let mut nodes = nodes.write().await;
                let node = nodes
                    .nodes
                    .get_mut(&node_id)
                    .ok_or_else(|| anyhow!("No node exists with id `{node_id}`"))?;
                let is_running = node.status() == NodeStatus::Running;
                // Stopping a node that is not running is a no-op
                node.stop().await?;

                // If the fields we receive are populated, we update the node data.
                if let Some(name) = name {
                    node.data.name = name;
                }
                if let Some(self_update) = self_update {
                    node.data.self_update = self_update;
                }
                if !properties.is_empty() {
                    node.data.properties = properties.into_iter().map(|p| (p.name, p.value)).collect();
                }
                node.data.save().await?;

                if is_running {
                    node.start().await?;
                }
            }
            Command::InfoGet(_) => unimplemented!(),
            Command::Generic(_) => unimplemented!(),
        },
        None => bail!("Node command is `None`"),
    };

    Ok(())
}

pub fn with_auth<T>(inner: T, auth_token: &str) -> Request<T> {
    let mut request = Request::new(inner);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", base64::encode(auth_token))
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
