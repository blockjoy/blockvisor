use crate::{
    cluster::ClusterData,
    config::Config,
    linux_platform::LinuxPlatform,
    node::Node,
    node_data::{NodeImage, NodeStatus},
    nodes::{self, NodeConfig, Nodes},
    pal::{NetInterface, Pal},
    services::{api, api::pb},
    {get_bv_status, set_bv_status, utils, ServiceStatus}, {node_metrics, BV_VAR_PATH},
};
use chrono::Utc;
use eyre::{anyhow, Context};
use petname::Petnames;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::str::FromStr;
use std::{collections::HashMap, fmt::Debug, sync::Arc};
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{info, instrument};
use uuid::Uuid;

// Data that we display in cli
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeDisplayInfo {
    pub id: Uuid,
    pub name: String,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub status: NodeStatus,
    pub uptime: Option<i64>,
    pub standalone: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeCreateRequest {
    pub image: NodeImage,
    pub network: String,
    pub standalone: bool,
    pub ip: Option<String>,
    pub gateway: Option<String>,
    pub props: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CreateStandaloneNodeRequest {}

#[tonic_rpc::tonic_rpc(bincode)]
trait Service {
    fn info() -> String;
    fn health() -> ServiceStatus;
    fn start_update() -> ServiceStatus;
    fn get_node_status(id: Uuid) -> NodeStatus;
    fn get_node(id: Uuid) -> NodeDisplayInfo;
    fn get_nodes() -> Vec<NodeDisplayInfo>;
    fn create_node(request: NodeCreateRequest) -> NodeDisplayInfo;
    fn upgrade_node(id: Uuid, image: NodeImage);
    fn start_node(id: Uuid);
    fn stop_node(id: Uuid, force: bool);
    fn delete_node(id: Uuid);
    fn get_node_jobs(id: Uuid) -> Vec<(String, babel_api::engine::JobInfo)>;
    fn get_node_job_info(id: Uuid, job_name: String) -> babel_api::engine::JobInfo;
    fn start_node_job(id: Uuid, job_name: String);
    fn stop_node_job(id: Uuid, job_name: String);
    fn cleanup_node_job(id: Uuid, job_name: String);
    fn get_node_logs(id: Uuid) -> Vec<String>;
    fn get_babel_logs(id: Uuid, max_lines: u32) -> Vec<String>;
    fn get_node_keys(id: Uuid) -> Vec<String>;
    fn get_node_id_for_name(name: String) -> String;
    fn list_capabilities(id: Uuid) -> Vec<String>;
    fn run(id: Uuid, method: String, param: String) -> String;
    fn get_node_metrics(id: Uuid) -> node_metrics::Metric;
    fn get_cluster_status() -> String; // TODO: update with proper struct
}

pub struct State<P: Pal + Debug> {
    pub nodes: Arc<Nodes<P>>,
    pub cluster: Arc<Option<ClusterData>>,
    pub dev_mode: bool,
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
    async fn info(&self, _request: Request<()>) -> Result<Response<String>, Status> {
        let pal = LinuxPlatform::new().map_err(|e| Status::internal(format!("{e:#}")))?;
        Ok(Response::new(format!(
            "{} {} - {:?}\n BV_PATH: {}\n BABEL_PATH: {}\n JOB_RUNNER_PATH: {}\n CONFIG: {:#?}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            get_bv_status().await,
            pal.bv_root().join(BV_VAR_PATH).to_string_lossy(),
            pal.babel_path().to_string_lossy(),
            pal.job_runner_path().to_string_lossy(),
            Config::load(pal.bv_root())
                .await
                .map_err(|e| Status::internal(format!("{e:#}")))?,
        )))
    }

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
        request: Request<NodeCreateRequest>,
    ) -> Result<Response<NodeDisplayInfo>, Status> {
        status_check().await?;
        let req = request.into_inner();
        let standalone = req.standalone || self.dev_mode;
        if !standalone && (req.ip.is_some() || req.gateway.is_some()) {
            return Err(Status::invalid_argument(
                "custom ip and gateway is allowed only in standalone mode",
            ));
        }
        Ok(Response::new(if standalone {
            self.create_standalone_node(req)
                .await
                .map_err(|err| Status::unknown(format!("{err:#}")))?
        } else {
            self.create_node_with_api(req)
                .await
                .map_err(|err| Status::unknown(format!("{err:#}")))?
        }))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn upgrade_node(
        &self,
        request: Request<(Uuid, NodeImage)>,
    ) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, image) = request.into_inner();

        if self.is_standalone_node(id).await? {
            self.nodes
                .upgrade(id, image)
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
            Ok(Response::new(()))
        } else {
            Err(Status::unimplemented(
                "non-standalone nodes are upgraded automatically by API,\
             manual trigger for upgrade is not implemented yet",
            ))
        }
    }

    #[instrument(skip(self), ret(Debug))]
    async fn delete_node(&self, request: Request<Uuid>) -> Result<Response<()>, Status> {
        status_check().await?;
        let id = request.into_inner();
        if self.is_standalone_node(id).await? {
            self.nodes
                .delete(id)
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
        } else {
            self.connect_to_node_service()
                .await?
                .delete(pb::NodeServiceDeleteRequest { id: id.to_string() })
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
        }
        Ok(Response::new(()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn start_node(&self, request: Request<Uuid>) -> Result<Response<()>, Status> {
        status_check().await?;
        let id = request.into_inner();
        if self.is_standalone_node(id).await? {
            self.nodes
                .start(id, true)
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
        } else {
            self.connect_to_node_service()
                .await?
                .start(pb::NodeServiceStartRequest { id: id.to_string() })
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
        }
        Ok(Response::new(()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn stop_node(&self, request: Request<(Uuid, bool)>) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, force) = request.into_inner();
        if self.is_standalone_node(id).await? {
            self.nodes
                .stop(id, force)
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
        } else {
            self.connect_to_node_service()
                .await?
                .stop(pb::NodeServiceStopRequest { id: id.to_string() })
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
        }
        Ok(Response::new(()))
    }

    #[instrument(skip(self))]
    async fn get_node_jobs(
        &self,
        request: Request<Uuid>,
    ) -> Result<Response<Vec<(String, babel_api::engine::JobInfo)>>, Status> {
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
    async fn get_node_job_info(
        &self,
        request: Request<(Uuid, String)>,
    ) -> Result<Response<babel_api::engine::JobInfo>, Status> {
        status_check().await?;
        let (id, job_name) = request.into_inner();
        let info = self
            .nodes
            .job_info(id, &job_name)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(info))
    }

    #[instrument(skip(self))]
    async fn start_node_job(
        &self,
        request: Request<(Uuid, String)>,
    ) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, job_name) = request.into_inner();
        self.nodes
            .start_job(id, &job_name)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self))]
    async fn stop_node_job(
        &self,
        request: Request<(Uuid, String)>,
    ) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, job_name) = request.into_inner();
        self.nodes
            .stop_job(id, &job_name)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self))]
    async fn cleanup_node_job(
        &self,
        request: Request<(Uuid, String)>,
    ) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, job_name) = request.into_inner();
        self.nodes
            .cleanup_job(id, &job_name)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
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

    #[instrument(skip(self), ret(Debug))]
    async fn get_cluster_status(&self, _request: Request<()>) -> Result<Response<String>, Status> {
        status_check().await?;
        let status = if let Some(ref cluster) = *self.cluster {
            let chitchat = cluster.chitchat.lock().await;
            json!({"cluster_id": chitchat.cluster_id().to_string(),
                "cluster_state": chitchat.state_snapshot(),
                "live_hosts": chitchat.live_nodes().cloned().collect::<Vec<_>>(),
                "dead_hosts": chitchat.dead_nodes().cloned().collect::<Vec<_>>(),
            })
            .to_string()
        } else {
            "None".to_string()
        };
        Ok(Response::new(status))
    }
}

impl<P> State<P>
where
    P: 'static + Debug + Pal + Send + Sync,
    P::NetInterface: 'static + Send + Sync,
    P::NodeConnection: 'static + Send + Sync,
    P::VirtualMachine: 'static + Send + Sync,
{
    async fn is_standalone_node(&self, id: Uuid) -> eyre::Result<bool, Status> {
        Ok(self.dev_mode
            || self
                .nodes
                .nodes
                .read()
                .await
                .get(&id)
                .ok_or_else(|| Status::not_found(format!("node '{id}' not found")))?
                .read()
                .await
                .data
                .standalone)
    }

    async fn connect_to_node_service(&self) -> Result<api::NodesServiceClient, Status> {
        api::connect_to_api_service(
            &self.nodes.api_config,
            pb::node_service_client::NodeServiceClient::with_interceptor,
        )
        .await
        .map_err(|e| Status::unknown(format!("Error connecting to api: {e:#}")))
    }

    async fn get_node_display_info(
        &self,
        id: Uuid,
        node_lock: &RwLock<Node<P>>,
    ) -> eyre::Result<NodeDisplayInfo> {
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
                standalone: node.data.standalone,
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
                standalone: cache.standalone,
            }
        })
    }

    async fn create_standalone_node(
        &self,
        req: NodeCreateRequest,
    ) -> eyre::Result<NodeDisplayInfo> {
        let id = Uuid::new_v4();
        let mut used_ips = vec![];
        for (_, node) in self.nodes.nodes.read().await.iter() {
            used_ips.push(node.read().await.data.network_interface.ip().to_string());
        }
        let props: HashMap<String, String> = req
            .props
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        let properties = props
            .into_iter()
            .chain([("network".to_string(), req.network.clone())])
            .collect();
        let net = utils::discover_net_params(&self.nodes.api_config.read().await.iface)
            .await
            .unwrap_or_default();
        let ip = match req.ip {
            None => {
                let ip = utils::next_available_ip(&net, &used_ips).map_err(|err| {
                    anyhow!("failed to auto assign ip - provide it manually : {err}")
                })?;
                info!("Auto-assigned ip `{ip}` for node '{id}'");
                ip
            }
            Some(ip) => ip,
        };
        let name = Petnames::default().generate_one(3, "_");
        let gateway = match req.gateway {
            None => {
                let gateway = net
                    .gateway
                    .ok_or(anyhow!("can't auto discover gateway - provide it manually",))?;
                info!("Auto-discovered gateway `{gateway} for node '{id}'");
                gateway
            }
            Some(gateway) => gateway,
        };
        self.nodes
            .create(
                id,
                NodeConfig {
                    name: name.clone(),
                    image: req.image.clone(),
                    ip: ip.clone(),
                    gateway: gateway.clone(),
                    properties,
                    network: req.network,
                    rules: vec![],
                    standalone: true,
                },
            )
            .await?;
        Ok(NodeDisplayInfo {
            id,
            name,
            image: req.image,
            ip,
            gateway,
            standalone: true,
            status: NodeStatus::Stopped,
            uptime: None,
        })
    }

    async fn create_node_with_api(&self, req: NodeCreateRequest) -> eyre::Result<NodeDisplayInfo> {
        // map properties into api format
        let properties = req
            .props
            .as_deref()
            .map(serde_json::from_str::<HashMap<String, String>>)
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(|(key, value)| pb::NodeProperty {
                name: key.clone(),
                display_name: format!("BV CLI {key}"),
                ui_type: pb::UiType::Text.into(),
                disabled: false,
                required: false,
                value,
            })
            .collect();

        // get org_id associated with this host
        let host_id = self.nodes.api_config.read().await.id;
        let org_id = self.nodes.api_config.org_id().await?;

        // get blockchain id for given image
        let mut blockchain_client = api::connect_to_api_service(
            &self.nodes.api_config,
            pb::blockchain_service_client::BlockchainServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "error connecting to api")?;
        let blockchains = blockchain_client
            .list(pb::BlockchainServiceListRequest {
                org_id: Some(org_id.clone()),
            })
            .await?
            .into_inner();
        let blockchain_id = blockchains
            .blockchains
            .into_iter()
            .find(|blockchain| blockchain.name == req.image.protocol)
            .ok_or(anyhow!(
                "blockchain id not found for {}",
                req.image.protocol
            ))?
            .id;

        let mut node_client = self.connect_to_node_service().await?;
        let node = node_client
            .create(pb::NodeServiceCreateRequest {
                org_id,
                blockchain_id,
                version: req.image.node_version.clone(),
                node_type: pb::NodeType::from_str(&req.image.node_type)?.into(),
                properties,
                network: req.network,
                placement: Some(pb::NodePlacement {
                    placement: Some(pb::node_placement::Placement::HostId(host_id)),
                }),
                allow_ips: vec![],
                deny_ips: vec![],
            })
            .await?
            .into_inner()
            .node
            .ok_or_else(|| anyhow!("empty node create response from API"))?;

        Ok(NodeDisplayInfo {
            id: Uuid::parse_str(&node.id).with_context(|| {
                format!("node_create received invalid node id from API: {}", node.id)
            })?,
            name: node.name,
            image: req.image,
            ip: node.ip,
            gateway: node.ip_gateway,
            status: NodeStatus::Stopped,
            uptime: None,
            standalone: false,
        })
    }
}
