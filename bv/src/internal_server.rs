use crate::node::Node;
use crate::node_data::{NodeDisplayInfo, NodeImage, NodeStatus};
use crate::node_metrics;
use crate::nodes::{self, Nodes};
use crate::pal::{NetInterface, Pal};
use crate::{get_bv_status, set_bv_status, ServiceStatus};
use chrono::Utc;
use std::fmt::Debug;
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::instrument;
use uuid::Uuid;

#[tonic_rpc::tonic_rpc(bincode)]
trait Service {
    fn health() -> ServiceStatus;
    fn start_update() -> ServiceStatus;
    fn get_node_status(id: Uuid) -> NodeStatus;
    fn get_node(id: Uuid) -> NodeDisplayInfo;
    fn get_nodes() -> Vec<NodeDisplayInfo>;
    fn create_node(id: Uuid, config: nodes::NodeConfig);
    fn upgrade_node(id: Uuid, image: NodeImage);
    fn start_node(id: Uuid);
    fn stop_node(id: Uuid);
    fn delete_node(id: Uuid);
    fn get_node_jobs(id: Uuid) -> Vec<(String, babel_api::engine::JobStatus)>;
    fn get_node_logs(id: Uuid) -> Vec<String>;
    fn get_babel_logs(id: Uuid, max_lines: u32) -> Vec<String>;
    fn get_node_keys(id: Uuid) -> Vec<String>;
    fn get_node_id_for_name(name: String) -> String;
    fn list_capabilities(id: Uuid) -> Vec<String>;
    fn run(id: Uuid, method: String, param: String) -> String;
    fn get_node_metrics(id: Uuid) -> node_metrics::Metric;
}

pub struct State<P: Pal + Debug> {
    pub nodes: Arc<Nodes<P>>,
}

async fn status_check() -> Result<(), Status> {
    match get_bv_status().await {
        ServiceStatus::Undefined => Err(Status::unavailable("service not ready, try again later")),
        ServiceStatus::Updating => Err(Status::unavailable("pending update, try again later")),
        ServiceStatus::Broken => Err(Status::internal("service is broken, call support")),
        ServiceStatus::Ok => Ok(()),
    }
}

#[tonic::async_trait]
impl<P> service_server::Service for State<P>
where
    P: Pal + Debug + Send + Sync + 'static,
    P::NetInterface: Send + Sync + 'static,
    P::NodeConnection: Send + Sync + 'static,
    P::VirtualMachine: Send + Sync + 'static,
{
    #[instrument(skip(self), ret(Debug))]
    async fn health(&self, _request: Request<()>) -> Result<Response<ServiceStatus>, Status> {
        Ok(Response::new(get_bv_status().await))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn start_update(&self, _request: Request<()>) -> Result<Response<ServiceStatus>, Status> {
        set_bv_status(ServiceStatus::Updating).await;
        Ok(Response::new(ServiceStatus::Updating))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_status(
        &self,
        request: Request<Uuid>,
    ) -> Result<Response<NodeStatus>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let status = self
            .nodes
            .status(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(status))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node(&self, request: Request<Uuid>) -> Result<Response<NodeDisplayInfo>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let nodes_lock = self.nodes.nodes.read().await;
        if let Some(node_lock) = nodes_lock.get(&id) {
            Ok(Response::new(
                self.get_node_display_info(id, node_lock)
                    .await
                    .map_err(|e| Status::unknown(format!("{e:#}")))?,
            ))
        } else {
            Err(Status::not_found(format!("Node {id} not found")))
        }
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_nodes(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Vec<NodeDisplayInfo>>, Status> {
        status_check().await?;
        let nodes_lock = self.nodes.nodes.read().await;
        let mut nodes = vec![];
        for (id, node_lock) in nodes_lock.iter() {
            nodes.push(
                self.get_node_display_info(*id, node_lock)
                    .await
                    .map_err(|e| Status::unknown(format!("{e:#}")))?,
            );
        }
        Ok(Response::new(nodes))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn create_node(
        &self,
        request: Request<(Uuid, nodes::NodeConfig)>,
    ) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, config) = request.into_inner();
        self.nodes
            .create(id, config)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn upgrade_node(
        &self,
        request: Request<(Uuid, NodeImage)>,
    ) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, image) = request.into_inner();
        self.nodes
            .upgrade(id, image)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn delete_node(&self, request: Request<Uuid>) -> Result<Response<()>, Status> {
        status_check().await?;
        let id = request.into_inner();
        self.nodes
            .delete(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn start_node(&self, request: Request<Uuid>) -> Result<Response<()>, Status> {
        status_check().await?;
        let id = request.into_inner();
        self.nodes
            .start(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn stop_node(&self, request: Request<Uuid>) -> Result<Response<()>, Status> {
        status_check().await?;
        let id = request.into_inner();
        self.nodes
            .stop(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self))]
    async fn get_node_jobs(
        &self,
        request: Request<Uuid>,
    ) -> Result<Response<Vec<(String, babel_api::engine::JobStatus)>>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let jobs = self
            .nodes
            .jobs(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(jobs))
    }

    #[instrument(skip(self))]
    async fn get_node_logs(&self, request: Request<Uuid>) -> Result<Response<Vec<String>>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let logs = self
            .nodes
            .logs(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(logs))
    }

    #[instrument(skip(self))]
    async fn get_babel_logs(
        &self,
        request: Request<(Uuid, u32)>,
    ) -> Result<Response<Vec<String>>, Status> {
        status_check().await?;
        let (id, max_lines) = request.into_inner();
        let logs = self
            .nodes
            .babel_logs(id, max_lines)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;

        Ok(Response::new(logs))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_keys(&self, request: Request<Uuid>) -> Result<Response<Vec<String>>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let keys = self
            .nodes
            .keys(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        let names = keys.into_iter().map(|k| k.name).collect();
        Ok(Response::new(names))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_id_for_name(
        &self,
        request: Request<String>,
    ) -> Result<Response<String>, Status> {
        status_check().await?;
        let name = request.into_inner();
        let id = self
            .nodes
            .node_id_for_name(&name)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(id.to_string()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn list_capabilities(
        &self,
        request: Request<Uuid>,
    ) -> Result<Response<Vec<String>>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let capabilities = self
            .nodes
            .capabilities(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(capabilities))
    }

    /// Calls an arbitrary method on a the blockchain node running inside the VM.
    #[instrument(skip(self), ret(Debug))]
    async fn run(
        &self,
        request: Request<(Uuid, String, String)>,
    ) -> Result<Response<String>, Status> {
        status_check().await?;
        let (id, method, param) = request.into_inner();
        let value = self
            .nodes
            .call_method(id, &method, &param, true)
            .await
            .map_err(|e| match e {
                nodes::BabelError::MethodNotFound => {
                    Status::not_found("blockchain method not found")
                }
                nodes::BabelError::Internal { err } => Status::internal(format!("{err:#}")),
                nodes::BabelError::Plugin { err } => Status::unknown(format!("{err:#}")),
            })?;
        Ok(Response::new(value))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_metrics(
        &self,
        request: Request<Uuid>,
    ) -> Result<Response<node_metrics::Metric>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let metrics = self
            .nodes
            .metrics(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(metrics))
    }
}

impl<P> State<P>
where
    P: 'static + Debug + Pal + Send + Sync,
    P::NetInterface: 'static + Send + Sync,
    P::NodeConnection: 'static + Send + Sync,
    P::VirtualMachine: 'static + Send + Sync,
{
    async fn get_node_display_info(
        &self,
        id: Uuid,
        node_lock: &RwLock<Node<P>>,
    ) -> anyhow::Result<NodeDisplayInfo> {
        Ok(if let Ok(node) = node_lock.try_read() {
            let status = node.status();
            NodeDisplayInfo {
                id: node.data.id,
                name: node.data.name.clone(),
                image: node.data.image.clone(),
                status,
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
                .node_data_cache(id)
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
            NodeDisplayInfo {
                id,
                name: cache.name,
                image: cache.image,
                status: NodeStatus::Busy,
                ip: cache.ip,
                gateway: cache.gateway,
                uptime: cache
                    .started_at
                    .map(|dt| Utc::now().signed_duration_since(dt).num_seconds()),
            }
        })
    }
}
