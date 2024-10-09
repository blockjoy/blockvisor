use crate::{
    apptainer_platform::ApptainerPlatform,
    bv_config,
    bv_config::SharedConfig,
    cluster::ClusterData,
    hosts,
    node_state::{NodeProperties, NodeState, ProtocolImageKey, VmStatus},
    nodes_manager::{self, MaybeNode, NodesManager},
    pal::Pal,
    protocol,
    services::{
        self,
        api::{self, common, pb},
    },
    {get_bv_status, set_bv_status, ServiceStatus}, {node_metrics, BV_VAR_PATH},
};
use babel_api::engine::JobsInfo;
use eyre::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{fmt::Debug, sync::Arc};
use tonic::{Request, Response, Status};
use tracing::instrument;
use uuid::Uuid;

// Data that we display in cli
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NodeDisplayInfo {
    pub status: VmStatus,
    pub state: NodeState,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CreateNodeRequest {
    pub protocol_image_key: ProtocolImageKey,
    pub image_version: Option<String>,
    pub build_version: Option<u64>,
    pub properties: NodeProperties,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CreateDevNodeRequest {
    pub new_node_state: NodeState,
}

#[tonic_rpc::tonic_rpc(bincode)]
trait Service {
    fn info() -> String;
    fn health() -> ServiceStatus;
    fn start_update() -> ServiceStatus;
    fn get_host_metrics() -> hosts::HostMetrics;
    fn get_node_status(id: Uuid) -> VmStatus;
    fn get_node(id: Uuid) -> NodeDisplayInfo;
    fn get_nodes() -> Vec<NodeDisplayInfo>;
    fn create_node(req: CreateNodeRequest) -> NodeDisplayInfo;
    fn create_dev_node(req: CreateDevNodeRequest) -> NodeDisplayInfo;
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
        let mut config = bv_config::Config::load(pal.bv_root())
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?;
        config.api_config.token = "***".to_string();
        config.api_config.refresh_token = "***".to_string();
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
    async fn get_node_status(&self, request: Request<Uuid>) -> Result<Response<VmStatus>, Status> {
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
        request: Request<CreateNodeRequest>,
    ) -> Result<Response<NodeDisplayInfo>, Status> {
        status_check().await?;
        Ok(Response::new(
            self.create_node(request.into_inner())
                .await
                .map_err(|err| Status::unknown(format!("{err:#}")))?,
        ))
    }

    #[instrument(skip(self), ret(Debug))]
    async fn create_dev_node(
        &self,
        request: Request<CreateDevNodeRequest>,
    ) -> Result<Response<NodeDisplayInfo>, Status> {
        status_check().await?;
        Ok(Response::new(
            self.create_dev_node(request.into_inner().new_node_state)
                .await
                .map_err(|err| Status::unknown(format!("{err:#}")))?,
        ))
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
                .start(pb::NodeServiceStartRequest {
                    node_id: id.to_string(),
                })
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
                .stop(pb::NodeServiceStopRequest {
                    node_id: id.to_string(),
                })
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
                .delete(pb::NodeServiceDeleteRequest {
                    node_id: id.to_string(),
                })
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

    /// Calls an arbitrary method on the node running inside the VM.
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
                    Status::not_found("protocol method not found")
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
                        state: node.state.clone(),
                        status,
                    }
                } else {
                    let cache = self
                        .nodes_manager
                        .node_state_cache(id)
                        .await
                        .map_err(|e| Status::unknown(format!("{e:#}")))?;
                    NodeDisplayInfo {
                        state: cache,
                        status: VmStatus::Busy,
                    }
                }
            }
            MaybeNode::BrokenNode(state) => NodeDisplayInfo {
                state: state.clone(),
                status: VmStatus::Failed,
            },
        })
    }

    async fn create_node(&self, req: CreateNodeRequest) -> eyre::Result<NodeDisplayInfo> {
        if self.dev_mode {
            bail!("blockvisord-dev support only dev nodes")
        }
        // map properties into api format
        let properties = req
            .properties
            .clone()
            .into_iter()
            .map(|(key, value)| common::ImagePropertyValue { key, value })
            .collect::<Vec<_>>();
        let (host_id, org_id) = self.get_host_and_org_id().await?;
        let image_id = self
            .get_image_id(
                &req.protocol_image_key.protocol_key,
                &req.protocol_image_key.variant_key,
                req.image_version,
                req.build_version,
            )
            .await?;

        let mut node_client = self.connect_to_node_service().await?;
        let mut created_nodes = node_client
            .create(pb::NodeServiceCreateRequest {
                old_node_id: None,
                org_id,
                placement: Some(common::NodePlacement {
                    placement: Some(common::node_placement::Placement::HostId(host_id)),
                }),
                changes: properties,
                image_id,
                add_rules: vec![],
                tags: None,
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
            state: node.try_into()?,
            status: VmStatus::Stopped,
        })
    }

    async fn create_dev_node(
        &self,
        mut new_node_state: NodeState,
    ) -> eyre::Result<NodeDisplayInfo> {
        new_node_state.dev_mode = true;
        Ok(NodeDisplayInfo {
            state: self.nodes_manager.create(new_node_state).await?,
            status: VmStatus::Stopped,
        })
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
                    host_id: host_id.clone(),
                })
                .await
                .with_context(|| "can't fetch host organization id")?
                .into_inner()
                .host
                .ok_or(anyhow!("host {host_id} not found in API"))?
                .org_id,
        ))
    }

    /// Find image id by protocol name and version.
    async fn get_image_id(
        &self,
        protocol: &str,
        variant: &str,
        version: Option<String>,
        build_version: Option<u64>,
    ) -> eyre::Result<String> {
        Ok(
            services::protocol::ProtocolService::new(services::DefaultConnector {
                config: self.config.clone(),
            })
            .await?
            .get_image(
                protocol::ImageKey {
                    protocol_key: protocol.to_string(),
                    variant_key: variant.to_string(),
                },
                version,
                build_version,
            )
            .await?
            .ok_or(anyhow!("image not found"))?
            .image_id,
        )
    }
}
