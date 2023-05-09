pub mod bv_pb {
    tonic::include_proto!("blockjoy.blockvisor.v1");
}

use crate::pal::{NetInterface, Pal};
use crate::{
    get_bv_status,
    node_data::{NodeImage, NodeStatus},
    nodes,
    nodes::Nodes,
    set_bv_status,
};
use chrono::Utc;
use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::instrument;

async fn status_check() -> Result<(), Status> {
    match get_bv_status().await {
        bv_pb::ServiceStatus::UndefinedServiceStatus => {
            Err(Status::unavailable("service not ready, try again later"))
        }
        bv_pb::ServiceStatus::Updating => {
            Err(Status::unavailable("pending update, try again later"))
        }
        bv_pb::ServiceStatus::Broken => Err(Status::internal("service is broken, call support")),
        bv_pb::ServiceStatus::Ok => Ok(()),
    }
}

pub struct BlockvisorServer<P: Pal + Debug> {
    pub nodes: Arc<Nodes<P>>,
}

#[tonic::async_trait]
impl<P> bv_pb::blockvisor_server::Blockvisor for BlockvisorServer<P>
where
    P: Pal + Debug + Send + Sync + 'static,
    <P as Pal>::NetInterface: Send + Sync + 'static,
{
    #[instrument(skip(self), ret(Debug))]
    async fn health(
        &self,
        _request: Request<bv_pb::HealthRequest>,
    ) -> Result<Response<bv_pb::HealthResponse>, Status> {
        let mut reply = bv_pb::HealthResponse { status: 0 };
        reply.set_status(get_bv_status().await);
        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn start_update(
        &self,
        _request: Request<bv_pb::StartUpdateRequest>,
    ) -> Result<Response<bv_pb::StartUpdateResponse>, Status> {
        set_bv_status(bv_pb::ServiceStatus::Updating).await;
        Ok(Response::new(bv_pb::StartUpdateResponse {
            status: bv_pb::ServiceStatus::Updating.into(),
        }))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn create_node(
        &self,
        request: Request<bv_pb::CreateNodeRequest>,
    ) -> Result<Response<bv_pb::CreateNodeResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;
        let image: NodeImage = request
            .image
            .ok_or_else(|| Status::invalid_argument("Image not provided"))?
            .into();
        let properties = request
            .properties
            .into_iter()
            .map(|p| (p.name, p.value))
            .collect();

        self.nodes
            .create(
                id,
                nodes::NodeConfig {
                    name: request.name,
                    image,
                    ip: request.ip,
                    gateway: request.gateway,
                    rules: vec![],
                    properties,
                },
            )
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::CreateNodeResponse {};

        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn upgrade_node(
        &self,
        request: Request<bv_pb::UpgradeNodeRequest>,
    ) -> Result<Response<bv_pb::UpgradeNodeResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;
        let image: NodeImage = request
            .image
            .ok_or_else(|| Status::invalid_argument("Image not provided"))?
            .into();

        self.nodes
            .upgrade(id, image)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::UpgradeNodeResponse {};

        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn delete_node(
        &self,
        request: Request<bv_pb::DeleteNodeRequest>,
    ) -> Result<Response<bv_pb::DeleteNodeResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        self.nodes
            .delete(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::DeleteNodeResponse {};
        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn start_node(
        &self,
        request: Request<bv_pb::StartNodeRequest>,
    ) -> Result<Response<bv_pb::StartNodeResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        self.nodes
            .start(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::StartNodeResponse {};

        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn stop_node(
        &self,
        request: Request<bv_pb::StopNodeRequest>,
    ) -> Result<Response<bv_pb::StopNodeResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        self.nodes
            .stop(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::StopNodeResponse {};

        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_nodes(
        &self,
        _request: Request<bv_pb::GetNodesRequest>,
    ) -> Result<Response<bv_pb::GetNodesResponse>, Status> {
        status_check().await?;
        let nodes_lock = self.nodes.nodes.read().await;
        let mut nodes = vec![];
        for (id, node_lock) in nodes_lock.iter() {
            let n = if let Ok(node) = node_lock.try_read() {
                let status = match node.status() {
                    NodeStatus::Running => bv_pb::NodeStatus::Running,
                    NodeStatus::Stopped => bv_pb::NodeStatus::Stopped,
                    NodeStatus::Failed => bv_pb::NodeStatus::Failed,
                };
                bv_pb::Node {
                    id: node.data.id.to_string(),
                    name: node.data.name.clone(),
                    image: Some(node.data.image.clone().into()),
                    status: status.into(),
                    ip: node.data.network_interface.ip().to_string(),
                    gateway: node.data.network_interface.gateway().to_string(),
                    uptime: node
                        .data
                        .started_at
                        .map(|dt| Utc::now().signed_duration_since(dt).num_seconds()),
                }
            } else {
                let cache = self
                    .nodes
                    .node_data_cache(*id)
                    .await
                    .map_err(|e| Status::unknown(e.to_string()))?;
                bv_pb::Node {
                    id: id.to_string(),
                    name: cache.name,
                    image: Some(cache.image.into()),
                    status: bv_pb::NodeStatus::Busy.into(),
                    ip: cache.ip,
                    gateway: cache.gateway,
                    uptime: cache
                        .started_at
                        .map(|dt| Utc::now().signed_duration_since(dt).num_seconds()),
                }
            };
            nodes.push(n);
        }

        let reply = bv_pb::GetNodesResponse { nodes };

        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_status(
        &self,
        request: Request<bv_pb::GetNodeStatusRequest>,
    ) -> Result<Response<bv_pb::GetNodeStatusResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        let status = self
            .nodes
            .status(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;
        let status = match status {
            NodeStatus::Running => bv_pb::NodeStatus::Running,
            NodeStatus::Stopped => bv_pb::NodeStatus::Stopped,
            NodeStatus::Failed => bv_pb::NodeStatus::Failed,
        };

        let reply = bv_pb::GetNodeStatusResponse {
            status: status.into(),
        };

        Ok(Response::new(reply))
    }

    #[instrument(skip(self))]
    async fn get_node_logs(
        &self,
        request: Request<bv_pb::GetNodeLogsRequest>,
    ) -> Result<Response<bv_pb::GetNodeLogsResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        let logs = self
            .nodes
            .logs(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        Ok(Response::new(bv_pb::GetNodeLogsResponse { logs }))
    }

    #[instrument(skip(self))]
    async fn get_babel_logs(
        &self,
        request: Request<bv_pb::GetBabelLogsRequest>,
    ) -> Result<Response<bv_pb::GetBabelLogsResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        let logs = self
            .nodes
            .babel_logs(id, request.max_lines)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        Ok(Response::new(bv_pb::GetBabelLogsResponse { logs }))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_keys(
        &self,
        request: Request<bv_pb::GetNodeKeysRequest>,
    ) -> Result<Response<bv_pb::GetNodeKeysResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        let keys = self
            .nodes
            .keys(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let names = keys.into_iter().map(|k| k.name).collect();
        Ok(Response::new(bv_pb::GetNodeKeysResponse { names }))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_id_for_name(
        &self,
        request: Request<bv_pb::GetNodeIdForNameRequest>,
    ) -> Result<Response<bv_pb::GetNodeIdForNameResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let name = request.name;

        let id = self
            .nodes
            .node_id_for_name(&name)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::GetNodeIdForNameResponse { id: id.to_string() };

        Ok(Response::new(reply))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn list_capabilities(
        &self,
        request: Request<bv_pb::ListCapabilitiesRequest>,
    ) -> Result<Response<bv_pb::ListCapabilitiesResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.node_id)?;

        let capabilities = self
            .nodes
            .capabilities(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        Ok(Response::new(bv_pb::ListCapabilitiesResponse {
            capabilities,
        }))
    }

    /// Calls an arbitrary method on a the blockchain node running inside the VM.
    #[instrument(skip(self), ret(Debug))]
    async fn blockchain(
        &self,
        request: Request<bv_pb::BlockchainRequest>,
    ) -> Result<Response<bv_pb::BlockchainResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.node_id)?;

        let value = self
            .nodes
            .call_method(id, &request.method, &request.param, true)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        Ok(Response::new(bv_pb::BlockchainResponse { value }))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_metrics(
        &self,
        request: Request<bv_pb::GetNodeMetricsRequest>,
    ) -> Result<Response<bv_pb::GetNodeMetricsResponse>, Status> {
        status_check().await?;
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.node_id)?;

        let metrics = self
            .nodes
            .metrics(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        Ok(Response::new(bv_pb::GetNodeMetricsResponse {
            height: metrics.height,
            block_age: metrics.block_age,
            staking_status: metrics.staking_status.map(|value| format!("{value:?}")),
            consensus: metrics.consensus,
            application_status: metrics.application_status.map(|value| format!("{value:?}")),
            sync_status: metrics.sync_status.map(|value| format!("{value:?}")),
        }))
    }
}

impl fmt::Display for bv_pb::NodeStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl fmt::Display for bv_pb::NodeImage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}",
            self.protocol, self.node_type, self.node_version
        )
    }
}

impl From<bv_pb::NodeImage> for NodeImage {
    fn from(image: bv_pb::NodeImage) -> Self {
        Self {
            protocol: image.protocol,
            node_type: image.node_type,
            node_version: image.node_version,
        }
    }
}

impl From<NodeImage> for bv_pb::NodeImage {
    fn from(image: NodeImage) -> Self {
        Self {
            protocol: image.protocol,
            node_type: image.node_type,
            node_version: image.node_version,
        }
    }
}

mod helpers {
    pub(super) fn parse_uuid(uuid: impl AsRef<str>) -> Result<uuid::Uuid, tonic::Status> {
        uuid.as_ref()
            .parse()
            .map_err(|_| tonic::Status::invalid_argument("Unparseable node id"))
    }
}
