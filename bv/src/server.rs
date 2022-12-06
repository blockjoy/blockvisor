pub mod bv_pb {
    // https://github.com/tokio-rs/prost/issues/661
    #![allow(clippy::derive_partial_eq_without_eq)]
    tonic::include_proto!("blockjoy.blockvisor.v1");
}

use crate::{node_data, node_data::NodeStatus, node_metrics, nodes::Nodes};
use std::sync::Arc;
use std::{fmt, str::FromStr};
use tokio::sync::Mutex;
use tonic::{transport::Endpoint, Request, Response, Status};

pub const BLOCKVISOR_SERVICE_PORT: usize = 9001;
pub const BLOCKVISOR_SERVICE_URL: &str = "http://localhost:9001";

lazy_static::lazy_static! {
    pub static ref BLOCKVISOR_SERVICE_ENDPOINT: Endpoint = Endpoint::from_str(BLOCKVISOR_SERVICE_URL).expect("valid url");
}

pub struct BlockvisorServer {
    pub nodes: Arc<Mutex<Nodes>>,
}

impl BlockvisorServer {
    pub async fn is_running() -> bool {
        Endpoint::connect(&BLOCKVISOR_SERVICE_ENDPOINT)
            .await
            .is_ok()
    }
}

#[tonic::async_trait]
impl bv_pb::blockvisor_server::Blockvisor for BlockvisorServer {
    async fn health(
        &self,
        _request: Request<bv_pb::HealthRequest>,
    ) -> Result<Response<bv_pb::HealthResponse>, Status> {
        let reply = bv_pb::HealthResponse {
            status: bv_pb::ServiceStatus::Ok.into(),
        };
        Ok(Response::new(reply))
    }

    async fn create_node(
        &self,
        request: Request<bv_pb::CreateNodeRequest>,
    ) -> Result<Response<bv_pb::CreateNodeResponse>, Status> {
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;
        let image = request
            .image
            .ok_or_else(|| Status::invalid_argument("Image not provided"))?;
        let image = node_data::NodeImage {
            protocol: image.protocol,
            node_type: image.node_type,
            node_version: image.node_version,
        };

        self.nodes
            .lock()
            .await
            .create(id, request.name, image, request.ip, request.gateway)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::CreateNodeResponse {};

        Ok(Response::new(reply))
    }

    async fn upgrade_node(
        &self,
        request: Request<bv_pb::UpgradeNodeRequest>,
    ) -> Result<Response<bv_pb::UpgradeNodeResponse>, Status> {
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;
        let image = request
            .image
            .ok_or_else(|| Status::invalid_argument("Image not provided"))?;
        let image = node_data::NodeImage {
            protocol: image.protocol,
            node_type: image.node_type,
            node_version: image.node_version,
        };

        self.nodes
            .lock()
            .await
            .upgrade(id, image)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::UpgradeNodeResponse {};

        Ok(Response::new(reply))
    }

    async fn delete_node(
        &self,
        request: Request<bv_pb::DeleteNodeRequest>,
    ) -> Result<Response<bv_pb::DeleteNodeResponse>, Status> {
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        self.nodes
            .lock()
            .await
            .delete(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::DeleteNodeResponse {};

        Ok(Response::new(reply))
    }

    async fn start_node(
        &self,
        request: Request<bv_pb::StartNodeRequest>,
    ) -> Result<Response<bv_pb::StartNodeResponse>, Status> {
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        self.nodes
            .lock()
            .await
            .start(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::StartNodeResponse {};

        Ok(Response::new(reply))
    }

    async fn stop_node(
        &self,
        request: Request<bv_pb::StopNodeRequest>,
    ) -> Result<Response<bv_pb::StopNodeResponse>, Status> {
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        self.nodes
            .lock()
            .await
            .stop(id)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::StopNodeResponse {};

        Ok(Response::new(reply))
    }

    async fn get_nodes(
        &self,
        _request: Request<bv_pb::GetNodesRequest>,
    ) -> Result<Response<bv_pb::GetNodesResponse>, Status> {
        let list = self.nodes.lock().await.list().await;
        let mut nodes = vec![];
        for node in list {
            let status = match node.status() {
                NodeStatus::Running => bv_pb::NodeStatus::Running,
                NodeStatus::Stopped => bv_pb::NodeStatus::Stopped,
                NodeStatus::Failed => bv_pb::NodeStatus::Failed,
            };
            let image = bv_pb::NodeImage {
                protocol: node.image.protocol,
                node_type: node.image.node_type,
                node_version: node.image.node_version,
            };
            let n = bv_pb::Node {
                id: node.id.to_string(),
                name: node.name,
                image: Some(image),
                status: status.into(),
                ip: node.network_interface.ip.to_string(),
                gateway: node.network_interface.gateway.to_string(),
            };
            nodes.push(n);
        }

        let reply = bv_pb::GetNodesResponse { nodes };

        Ok(Response::new(reply))
    }

    async fn get_node_status(
        &self,
        request: Request<bv_pb::GetNodeStatusRequest>,
    ) -> Result<Response<bv_pb::GetNodeStatusResponse>, Status> {
        let request = request.into_inner();
        let id = helpers::parse_uuid(request.id)?;

        let status = self
            .nodes
            .lock()
            .await
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

    async fn get_node_logs(
        &self,
        request: Request<bv_pb::GetNodeLogsRequest>,
    ) -> Result<Response<bv_pb::GetNodeLogsResponse>, Status> {
        let request = request.into_inner();
        let node_id = helpers::parse_uuid(request.id)?;
        let logs = self
            .nodes
            .lock()
            .await
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| Status::invalid_argument("No such node"))?
            .logs()
            .await
            .map_err(|e| Status::internal(&format!("Call to babel failed: `{e}`")))?;
        Ok(Response::new(bv_pb::GetNodeLogsResponse { logs }))
    }

    async fn get_node_keys(
        &self,
        request: Request<bv_pb::GetNodeKeysRequest>,
    ) -> Result<Response<bv_pb::GetNodeKeysResponse>, Status> {
        let request = request.into_inner();
        let node_id = helpers::parse_uuid(request.id)?;
        let keys = self
            .nodes
            .lock()
            .await
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| Status::invalid_argument("No such node"))?
            .download_keys()
            .await
            .map_err(|e| Status::internal(&format!("Call to babel failed: `{e}`")))?;
        let names = keys.into_iter().map(|k| k.name).collect();
        Ok(Response::new(bv_pb::GetNodeKeysResponse { names }))
    }

    async fn get_node_id_for_name(
        &self,
        request: Request<bv_pb::GetNodeIdForNameRequest>,
    ) -> Result<Response<bv_pb::GetNodeIdForNameResponse>, Status> {
        let request = request.into_inner();
        let name = request.name;

        let id = self
            .nodes
            .lock()
            .await
            .node_id_for_name(&name)
            .await
            .map_err(|e| Status::unknown(e.to_string()))?;

        let reply = bv_pb::GetNodeIdForNameResponse { id: id.to_string() };

        Ok(Response::new(reply))
    }

    async fn list_capabilities(
        &self,
        request: Request<bv_pb::ListCapabilitiesRequest>,
    ) -> Result<Response<bv_pb::ListCapabilitiesResponse>, Status> {
        let request = request.into_inner();
        let node_id = helpers::parse_uuid(request.node_id)?;
        let capabilities = self
            .nodes
            .lock()
            .await
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| Status::invalid_argument("No such node"))?
            .capabilities()
            .await
            .map_err(|e| Status::internal(&format!("Call to babel failed: `{e}`")))?;
        Ok(Response::new(bv_pb::ListCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn blockchain(
        &self,
        request: Request<bv_pb::BlockchainRequest>,
    ) -> Result<Response<bv_pb::BlockchainResponse>, Status> {
        let request = request.into_inner();
        let node_id = helpers::parse_uuid(request.node_id)?;
        let value = self
            .nodes
            .lock()
            .await
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| Status::invalid_argument("No such node"))?
            .call_method(&request.method)
            .await
            .map_err(|e| Status::internal(&format!("Call to babel failed: `{e}`")))?;
        Ok(Response::new(bv_pb::BlockchainResponse { value }))
    }

    async fn get_node_metrics(
        &self,
        request: Request<bv_pb::GetNodeMetricsRequest>,
    ) -> Result<Response<bv_pb::GetNodeMetricsResponse>, Status> {
        let request = request.into_inner();
        let node_id = helpers::parse_uuid(request.node_id)?;
        let mut node_lock = self.nodes.lock().await;
        let node = node_lock
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| Status::invalid_argument("No such node"))?;
        let (_, metrics) = node_metrics::collect_metric(node).await;
        Ok(Response::new(bv_pb::GetNodeMetricsResponse {
            height: metrics.height,
            block_age: metrics.block_age,
            staking_status: metrics.staking_status,
            consensus: metrics.consensus,
        }))
    }
}

impl fmt::Display for bv_pb::NodeStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
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

mod helpers {
    pub(super) fn parse_uuid(uuid: impl AsRef<str>) -> Result<uuid::Uuid, tonic::Status> {
        uuid.as_ref()
            .parse()
            .map_err(|_| tonic::Status::invalid_argument("Unparsable node id"))
    }
}
