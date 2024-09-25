use crate::{
    apptainer_platform::ApptainerPlatform,
    cluster::ClusterData,
    config,
    config::SharedConfig,
    hosts,
    node_state::{NodeImage, NodeProperties, NodeStatus},
    nodes_manager::{self, MaybeNode, NodeConfig, NodesManager},
    pal::Pal,
    services,
    services::api::{self, common, pb},
    {get_bv_status, set_bv_status, utils, ServiceStatus}, {node_metrics, BV_VAR_PATH},
};
use babel_api::engine::JobsInfo;
use babel_api::metadata::Requirements;
use chrono::Utc;
use eyre::{anyhow, Context};
use petname::Petnames;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::str::FromStr;
use std::{collections::HashMap, fmt::Debug, sync::Arc};
use tonic::{Request, Response, Status};
use tracing::{info, instrument};
use uuid::Uuid;

// Data that we display in cli
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeDisplayInfo {
    pub id: Uuid,
    pub name: String,
    pub image: NodeImage,
    pub network: String,
    pub ip: String,
    pub gateway: String,
    pub status: NodeStatus,
    pub uptime: Option<i64>,
    pub requirements: Option<Requirements>,
    pub properties: NodeProperties,
    pub assigned_cpus: Vec<usize>,
    pub dev_mode: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeCreateRequest {
    pub image: NodeImage,
    pub network: String,
    pub dev_mode: bool,
    pub ip: Option<String>,
    pub gateway: Option<String>,
    pub props: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CreateDevNodeRequest {}

#[tonic_rpc::tonic_rpc(bincode)]
trait Service {
    fn info() -> String;
    fn health() -> ServiceStatus;
    fn start_update() -> ServiceStatus;
    fn get_host_metrics() -> hosts::HostMetrics;
    fn get_node_status(id: Uuid) -> NodeStatus;
    fn get_node(id: Uuid) -> NodeDisplayInfo;
    fn get_nodes() -> Vec<NodeDisplayInfo>;
    fn create_node(request: NodeCreateRequest) -> NodeDisplayInfo;
    fn upgrade_node(id: Uuid, image: NodeImage);
    fn start_node(id: Uuid);
    fn stop_node(id: Uuid, force: bool);
    fn delete_node(id: Uuid);
    fn get_node_jobs(id: Uuid) -> JobsInfo;
    fn get_node_job_info(id: Uuid, job_name: String) -> babel_api::engine::JobInfo;
    fn start_node_job(id: Uuid, job_name: String);
    fn stop_node_job(id: Uuid, job_name: String);
    fn skip_node_job(id: Uuid, job_name: String);
    fn cleanup_node_job(id: Uuid, job_name: String);
    fn get_node_id_for_name(name: String) -> String;
    fn list_capabilities(id: Uuid) -> Vec<String>;
    fn run(id: Uuid, method: String, param: String) -> String;
    fn get_node_metrics(id: Uuid) -> node_metrics::Metric;
    fn get_cluster_status() -> String; // TODO: update with proper struct
}

pub struct State<P: Pal + Debug> {
    pub config: SharedConfig,
    pub nodes_manager: Arc<NodesManager<P>>,
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
    P::NodeConnection: Send + Sync + 'static,
    P::ApiServiceConnector: Send + Sync + 'static,
    P::VirtualMachine: Send + Sync + 'static,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    #[instrument(skip(self), ret(Debug))]
    async fn info(&self, _request: Request<()>) -> Result<Response<String>, Status> {
        let pal = ApptainerPlatform::default()
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?;
        let mut config = config::Config::load(pal.bv_root())
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?;
        config.token = "***".to_string();
        config.refresh_token = "***".to_string();
        let service_name = if self.dev_mode {
            format!("{}-dev", env!("CARGO_PKG_NAME"))
        } else {
            env!("CARGO_PKG_NAME").to_owned()
        };
        Ok(Response::new(format!(
            "{} {} - {:?}\n BV_PATH: {}\n BABEL_PATH: {}\n JOB_RUNNER_PATH: {}\n CONFIG: {:#?}",
            service_name,
            env!("CARGO_PKG_VERSION"),
            get_bv_status().await,
            pal.bv_root().join(BV_VAR_PATH).to_string_lossy(),
            pal.babel_path().to_string_lossy(),
            pal.job_runner_path().to_string_lossy(),
            config,
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
    async fn get_host_metrics(
        &self,
        _request: Request<()>,
    ) -> Result<Response<hosts::HostMetrics>, Status> {
        Ok(Response::new(
            hosts::HostMetrics::collect(
                self.nodes_manager.nodes_data_cache().await,
                self.nodes_manager.pal(),
            )
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?,
        ))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_status(
        &self,
        request: Request<Uuid>,
    ) -> Result<Response<NodeStatus>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let status = self
            .nodes_manager
            .status(id)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(status))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node(&self, request: Request<Uuid>) -> Result<Response<NodeDisplayInfo>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let nodes_lock = self.nodes_manager.nodes_list().await;
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
        let nodes_lock = self.nodes_manager.nodes_list().await;
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
        let dev_mode = req.dev_mode || self.dev_mode;
        if !dev_mode && (req.ip.is_some() || req.gateway.is_some()) {
            return Err(Status::invalid_argument(
                "custom ip and gateway is allowed only in dev mode",
            ));
        }
        Ok(Response::new(if dev_mode {
            self.create_dev_node(req)
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

        if self.is_dev_node(id).await? {
            self.nodes_manager
                .upgrade(id, image)
                .await
                .map_err(|e| Status::unknown(format!("{e:#}")))?;
            Ok(Response::new(()))
        } else {
            Err(Status::unimplemented(
                "non-dev nodes upgrade is managed by API, manual trigger for upgrade is not possible",
            ))
        }
    }

    #[instrument(skip(self), ret(Debug))]
    async fn start_node(&self, request: Request<Uuid>) -> Result<Response<()>, Status> {
        status_check().await?;
        let id = request.into_inner();
        if self.is_dev_node(id).await? {
            self.nodes_manager
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
        if self.is_dev_node(id).await? {
            self.nodes_manager
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

    #[instrument(skip(self), ret(Debug))]
    async fn delete_node(&self, request: Request<Uuid>) -> Result<Response<()>, Status> {
        status_check().await?;
        let id = request.into_inner();
        if self.is_dev_node(id).await? {
            self.nodes_manager
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

    #[instrument(skip(self))]
    async fn get_node_jobs(&self, request: Request<Uuid>) -> Result<Response<JobsInfo>, Status> {
        status_check().await?;
        let id = request.into_inner();
        let jobs = self
            .nodes_manager
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
            .nodes_manager
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
        self.nodes_manager
            .start_job(id, &job_name)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self))]
    async fn skip_node_job(
        &self,
        request: Request<(Uuid, String)>,
    ) -> Result<Response<()>, Status> {
        status_check().await?;
        let (id, job_name) = request.into_inner();
        self.nodes_manager
            .skip_job(id, &job_name)
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
        self.nodes_manager
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
        self.nodes_manager
            .cleanup_job(id, &job_name)
            .await
            .map_err(|e| Status::unknown(format!("{e:#}")))?;
        Ok(Response::new(()))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn get_node_id_for_name(
        &self,
        request: Request<String>,
    ) -> Result<Response<String>, Status> {
        status_check().await?;
        let name = request.into_inner();
        let id = self
            .nodes_manager
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
            .nodes_manager
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
            .nodes_manager
            .call_method(id, &method, &param, true)
            .await
            .map_err(|e| match e {
                nodes_manager::BabelError::MethodNotFound => {
                    Status::not_found("blockchain method not found")
                }
                nodes_manager::BabelError::Internal { err } => Status::internal(format!("{err:#}")),
                nodes_manager::BabelError::Plugin { err } => Status::unknown(format!("{err:#}")),
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
            .nodes_manager
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
    P: Pal + Send + Sync + Debug + 'static,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    async fn is_dev_node(&self, id: Uuid) -> eyre::Result<bool, Status> {
        Ok(self.dev_mode
            || match self
                .nodes_manager
                .nodes_list()
                .await
                .get(&id)
                .ok_or_else(|| Status::not_found(format!("node '{id}' not found")))?
            {
                MaybeNode::Node(node) => node.read().await.state.dev_mode,
                MaybeNode::BrokenNode(state) => state.dev_mode,
            })
    }

    async fn connect_to_node_service(&self) -> Result<api::NodesServiceClient, Status> {
        services::connect_to_api_service(
            &self.config,
            pb::node_service_client::NodeServiceClient::with_interceptor,
        )
        .await
        .map_err(|e| Status::unknown(format!("Error connecting to api: {e:#}")))
    }

    async fn get_node_display_info(
        &self,
        id: Uuid,
        maybe_node: &MaybeNode<P>,
    ) -> eyre::Result<NodeDisplayInfo> {
        Ok(match maybe_node {
            MaybeNode::Node(node_lock) => {
                if let Ok(node) = node_lock.try_read() {
                    let status = node.status().await;
                    NodeDisplayInfo {
                        id: node.state.id,
                        name: node.state.name.clone(),
                        image: node.state.image.clone(),
                        network: node.state.network.clone(),
                        status,
                        ip: node.state.network_interface.ip.to_string(),
                        gateway: node.state.network_interface.gateway.to_string(),
                        uptime: node
                            .state
                            .started_at
                            .map(|dt| Utc::now().signed_duration_since(dt).num_seconds()),
                        dev_mode: node.state.dev_mode,
                        requirements: Some(node.state.requirements.clone()),
                        properties: node.state.properties.clone(),
                        assigned_cpus: node.state.assigned_cpus.clone(),
                    }
                } else {
                    let cache = self
                        .nodes_manager
                        .node_state_cache(id)
                        .await
                        .map_err(|e| Status::unknown(format!("{e:#}")))?;
                    NodeDisplayInfo {
                        id,
                        name: cache.name,
                        image: cache.image,
                        network: cache.network,
                        status: NodeStatus::Busy,
                        ip: cache.ip,
                        gateway: cache.gateway,
                        uptime: cache
                            .started_at
                            .map(|dt| Utc::now().signed_duration_since(dt).num_seconds()),
                        dev_mode: cache.dev_mode,
                        requirements: Some(cache.requirements),
                        properties: cache.properties,
                        assigned_cpus: cache.assigned_cpus,
                    }
                }
            }
            MaybeNode::BrokenNode(state) => NodeDisplayInfo {
                id,
                name: state.name.clone(),
                image: state.image.clone(),
                network: state.network.clone(),
                status: NodeStatus::Failed,
                ip: state.network_interface.ip.to_string(),
                gateway: state.network_interface.gateway.to_string(),
                uptime: state
                    .started_at
                    .map(|dt| Utc::now().signed_duration_since(dt).num_seconds()),
                dev_mode: state.dev_mode,
                requirements: Some(state.requirements.clone()),
                properties: state.properties.clone(),
                assigned_cpus: state.assigned_cpus.clone(),
            },
        })
    }

    async fn create_dev_node(&self, req: NodeCreateRequest) -> eyre::Result<NodeDisplayInfo> {
        let id = Uuid::new_v4();
        let name = Petnames::default().generate_one(3, "-");
        let properties = parse_props(&req)?.into_iter().collect();
        let (ip, gateway) = self.discover_ip_and_gateway(&req, id).await?;
        let cache = self
            .nodes_manager
            .create(
                id,
                NodeConfig {
                    name: name.clone(),
                    image: req.image.clone(),
                    ip: ip.clone(),
                    gateway: gateway.clone(),
                    properties,
                    network: req.network.clone(),
                    rules: vec![],
                    dev_mode: true,
                    org_id: Default::default(),
                },
            )
            .await?;
        Ok(NodeDisplayInfo {
            id,
            name,
            image: req.image,
            network: req.network,
            ip,
            gateway,
            dev_mode: true,
            status: NodeStatus::Stopped,
            uptime: None,
            requirements: Some(cache.requirements),
            properties: cache.properties,
            assigned_cpus: cache.assigned_cpus,
        })
    }

    async fn create_node_with_api(&self, req: NodeCreateRequest) -> eyre::Result<NodeDisplayInfo> {
        // map properties into api format
        let properties = parse_props(&req)?
            .into_iter()
            .map(|(key, value)| pb::NodeProperty {
                name: key.clone(),
                display_name: format!("BV CLI {key}"),
                ui_type: common::UiType::Text.into(),
                disabled: false,
                required: false,
                value,
            })
            .collect::<Vec<_>>();
        let node_properties = properties
            .iter()
            .map(|p| (p.name.clone(), p.value.clone()))
            .collect();
        let (host_id, org_id) = self.get_host_and_org_id().await?;
        let blockchain_id = self.get_blockchain_id(&req.image.protocol).await?;

        let mut node_client = self.connect_to_node_service().await?;
        let mut created_nodes = node_client
            .create(pb::NodeServiceCreateRequest {
                old_node_id: None,
                org_id,
                blockchain_id,
                version: req.image.node_version.clone(),
                node_type: common::NodeType::from_str(&req.image.node_type)?.into(),
                properties,
                network: req.network.clone(),
                placement: Some(pb::NodePlacement {
                    placement: Some(pb::node_placement::Placement::HostId(host_id)),
                }),
                allow_ips: vec![],
                deny_ips: vec![],
            })
            .await
            .with_context(|| "create node via API failed")?
            .into_inner()
            .nodes;

        let node = match created_nodes.len() {
            0 => Err(anyhow!("empty node create response from API")),
            1 => Ok(created_nodes.pop().expect("one created node")),
            _ => Err(anyhow!("unexpected multiple node creation response")),
        }?;

        Ok(NodeDisplayInfo {
            id: Uuid::parse_str(&node.id).with_context(|| {
                format!("node_create received invalid node id from API: {}", node.id)
            })?,
            name: node.node_name,
            image: req.image,
            network: req.network,
            ip: node.ip,
            gateway: node.ip_gateway,
            status: NodeStatus::Stopped,
            uptime: None,
            dev_mode: false,
            requirements: None,
            properties: node_properties,
            assigned_cpus: vec![],
        })
    }

    /// Try to auto-discover ip and gateway for the node.
    async fn discover_ip_and_gateway(
        &self,
        req: &NodeCreateRequest,
        id: Uuid,
    ) -> eyre::Result<(String, String)> {
        let net = utils::discover_net_params(&self.config.read().await.iface)
            .await
            .unwrap_or_default();
        let gateway = match &req.gateway {
            None => {
                let gateway = net
                    .gateway
                    .clone()
                    .ok_or(anyhow!("can't auto discover gateway - provide it manually",))?;
                info!("Auto-discovered gateway `{gateway} for node '{id}'");
                gateway
            }
            Some(gateway) => gateway.clone(),
        };
        let ip = match &req.ip {
            None => {
                let mut used_ips = vec![];
                used_ips.push(gateway.clone());
                if let Some(host_ip) = &net.ip {
                    used_ips.push(host_ip.clone());
                }
                for (_, node) in self.nodes_manager.nodes_list().await.iter() {
                    used_ips.push(
                        match node {
                            MaybeNode::Node(node) => node.read().await.state.network_interface.ip,
                            MaybeNode::BrokenNode(state) => state.network_interface.ip,
                        }
                        .to_string(),
                    );
                }
                let ip = utils::next_available_ip(&net, &used_ips).map_err(|err| {
                    anyhow!("failed to auto assign ip - provide it manually : {err:#}")
                })?;
                info!("Auto-assigned ip `{ip}` for node '{id}'");
                ip
            }
            Some(ip) => ip.clone(),
        };
        Ok((ip, gateway))
    }

    /// Get org_id associated with this host.
    async fn get_host_and_org_id(&self) -> eyre::Result<(String, String)> {
        let host_id = self.config.read().await.id;
        let mut host_client = services::connect_to_api_service(
            &self.config,
            pb::host_service_client::HostServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "error connecting to api")?;
        Ok((
            host_id.clone(),
            host_client
                .get(pb::HostServiceGetRequest {
                    id: host_id.clone(),
                })
                .await
                .with_context(|| "can't fetch host organization id")?
                .into_inner()
                .host
                .ok_or(anyhow!("host {host_id} not found in API"))?
                .org_id,
        ))
    }

    /// Find blockchain id by protocol name.
    async fn get_blockchain_id(&self, protocol: &str) -> eyre::Result<String> {
        let protocol = protocol.to_lowercase();
        let mut blockchain_client = services::connect_to_api_service(
            &self.config,
            pb::blockchain_service_client::BlockchainServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "error connecting to api")?;
        let mut blockchains = blockchain_client
            .list(pb::BlockchainServiceListRequest {
                org_ids: vec![],
                offset: 0,
                limit: 16,
                search: Some(pb::BlockchainSearch {
                    operator: common::SearchOperator::And.into(),
                    id: None,
                    name: Some(protocol.clone()),
                    display_name: None,
                }),
                sort: vec![],
            })
            .await
            .with_context(|| "can't fetch blockchains list")?
            .into_inner();
        Ok(blockchains
            .blockchains
            .pop()
            .ok_or(anyhow!("blockchain id not found for {protocol}"))?
            .id)
    }
}

fn parse_props(req: &NodeCreateRequest) -> eyre::Result<HashMap<String, String>> {
    Ok(req
        .props
        .as_deref()
        .map(serde_json::from_str::<HashMap<String, String>>)
        .transpose()?
        .unwrap_or_default())
}
