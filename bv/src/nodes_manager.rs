use crate::{
    command_failed,
    commands::{self, into_internal, Error},
    config::SharedConfig,
    node::Node,
    node_context::build_registry_dir,
    node_data::{NodeData, NodeImage, NodeProperties, NodeStatus},
    node_metrics,
    pal::{NetInterface, Pal},
    scheduler,
    scheduler::{Scheduled, Scheduler},
    services::blockchain::ROOT_FS_FILE,
    services::{
        blockchain::{self, BlockchainService, BABEL_PLUGIN_NAME},
        kernel,
    },
    BV_VAR_PATH,
};
use babel_api::{
    engine::JobInfo,
    engine::JobsInfo,
    metadata::{firewall, BlockchainMetadata, Requirements},
    rhai_plugin,
};
use chrono::{DateTime, Utc};
use eyre::{anyhow, Context, Result};
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Debug,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::{
    fs::{self, read_dir},
    sync::{RwLock, RwLockReadGuard},
};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

pub const REGISTRY_CONFIG_FILENAME: &str = "nodes.json";
const MAX_SUPPORTED_RULES: usize = 128;

pub fn build_registry_filename(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_FILENAME)
}

#[derive(Debug)]
pub struct NodesManager<P: Pal + Debug> {
    api_config: SharedConfig,
    nodes: Arc<RwLock<HashMap<Uuid, RwLock<Node<P>>>>>,
    scheduler: Scheduler,
    node_data_cache: RwLock<HashMap<Uuid, NodeDataCache>>,
    node_ids: RwLock<HashMap<String, Uuid>>,
    state: RwLock<State>,
    registry_path: PathBuf,
    pal: Arc<P>,
}

/// Container with some shallow information about the node
///
/// This information is [mostly] immutable, and we can cache it for
/// easier access in case some node is locked, and we cannot access
/// it's actual data right away
#[derive(Clone, Debug, PartialEq)]
pub struct NodeDataCache {
    pub name: String,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub requirements: Requirements,
    pub started_at: Option<DateTime<Utc>>,
    pub standalone: bool,
}

pub type NodesDataCache = Vec<(Uuid, NodeDataCache)>;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NodeConfig {
    pub name: String,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub rules: Vec<firewall::Rule>,
    pub properties: NodeProperties,
    pub network: String,
    pub standalone: bool,
    pub org_id: String,
}

#[derive(Error, Debug)]
pub enum BabelError {
    #[error("given method not found")]
    MethodNotFound,
    #[error("BV plugin error: {err:#}")]
    Plugin { err: eyre::Error },
    #[error("BV internal error: {err:#}")]
    Internal { err: eyre::Error },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct State {
    machine_index: u32,
    #[serde(default)]
    pub scheduled_tasks: Vec<Scheduled>,
}

impl State {
    async fn load(registry_path: &Path) -> Result<Self> {
        info!(
            "Reading nodes common config file: {}",
            registry_path.display()
        );
        let config = fs::read_to_string(&registry_path)
            .await
            .context("failed to read nodes registry")?;
        serde_json::from_str(&config).context("failed to parse nodes registry")
    }

    async fn save(&self, registry_path: &Path) -> Result<()> {
        info!(
            "Writing nodes common config file: {}",
            registry_path.display()
        );
        let config = serde_json::to_string(self).map_err(into_internal)?;
        fs::write(registry_path, config)
            .await
            .map_err(into_internal)?;

        Ok(())
    }
}

impl<P> NodesManager<P>
where
    P: Pal + Send + Sync + Debug + 'static,
    P::NetInterface: Send + Sync + Clone,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    pub async fn load(pal: P, api_config: SharedConfig) -> Result<Self> {
        let bv_root = pal.bv_root();
        let registry_dir = build_registry_dir(bv_root);
        if !registry_dir.exists() {
            fs::create_dir_all(&registry_dir)
                .await
                .map_err(into_internal)?;
        }
        let registry_path = build_registry_filename(bv_root);
        let pal = Arc::new(pal);
        let nodes = Arc::new(RwLock::new(HashMap::new()));
        Ok(if registry_path.exists() {
            let data = State::load(&registry_path).await?;
            let scheduler = Scheduler::start(
                &data.scheduled_tasks,
                scheduler::NodeTaskHandler(nodes.clone()),
            );
            let (loaded_nodes, node_ids, node_data_cache) = Self::load_nodes(
                pal.clone(),
                api_config.clone(),
                &registry_dir,
                scheduler.tx(),
            )
            .await?;
            *nodes.write().await = loaded_nodes;
            Self {
                api_config,
                state: RwLock::new(data),
                nodes,
                scheduler,
                node_ids: RwLock::new(node_ids),
                node_data_cache: RwLock::new(node_data_cache),
                registry_path,
                pal,
            }
        } else {
            let scheduler = Scheduler::start(&[], scheduler::NodeTaskHandler(nodes.clone()));
            let nodes = Self {
                api_config,
                state: RwLock::new(State {
                    machine_index: 0,
                    scheduled_tasks: vec![],
                }),
                nodes,
                scheduler,
                node_ids: Default::default(),
                node_data_cache: Default::default(),
                registry_path,
                pal,
            };
            nodes.state.read().await.save(&nodes.registry_path).await?;
            nodes
        })
    }

    pub async fn detach(self) {
        let nodes_lock = self.nodes.read().await;
        for (id, node) in nodes_lock.iter() {
            if let Err(err) = node.write().await.detach().await {
                warn!("error while detaching node {id}: {err:#}")
            }
        }
        match self.scheduler.stop().await {
            Ok(tasks) => {
                let mut state = self.state.write().await;
                state.scheduled_tasks = tasks;
                if let Err(err) = state.save(&self.registry_path).await {
                    error!("error saving nodes state: {err:#}");
                }
            }
            Err(err) => error!("error stopping scheduler: {err:#}"),
        }
    }

    pub async fn nodes_list(&self) -> RwLockReadGuard<'_, HashMap<Uuid, RwLock<Node<P>>>> {
        self.nodes.read().await
    }

    pub async fn node_id_for_name(&self, name: &str) -> Result<Uuid> {
        let uuid = self
            .node_ids
            .read()
            .await
            .get(name)
            .copied()
            .ok_or_else(|| name_not_found(name))?;

        Ok(uuid)
    }

    #[instrument(skip(self))]
    pub async fn create(&self, id: Uuid, config: NodeConfig) -> commands::Result<()> {
        let mut node_ids = self.node_ids.write().await;
        if self.nodes.read().await.contains_key(&id) {
            warn!("Node with id `{id}` exists");
            return Ok(());
        }

        if node_ids.contains_key(&config.name) {
            command_failed!(Error::Internal(anyhow!(
                "Node with name `{}` exists",
                config.name
            )));
        }

        check_user_firewall_rules(&config.rules)?;

        let ip = config
            .ip
            .parse()
            .with_context(|| format!("invalid ip `{}`", config.ip))?;
        let gateway = config
            .gateway
            .parse()
            .with_context(|| format!("invalid gateway `{}`", config.gateway))?;

        let properties = config
            .properties
            .into_iter()
            .chain([("network".to_string(), config.network.clone())])
            .map(|(k, v)| (k.to_uppercase(), v))
            .collect();

        for n in self.nodes.read().await.values() {
            let node = n.read().await;
            if node.data.network_interface.ip() == &ip {
                command_failed!(Error::Internal(anyhow!(
                    "Node with ip address `{ip}` exists"
                )));
            }
        }

        let meta = Self::fetch_image_data(self.pal.clone(), self.api_config.clone(), &config.image)
            .await
            .with_context(|| "fetch image data failed")?;

        if !meta.nets.contains_key(&config.network) {
            command_failed!(Error::Internal(anyhow!(
                "invalid network name '{}'",
                config.network
            )));
        }

        self.check_node_requirements(&meta.requirements, &config.image, None)
            .await?;

        let network_interface = self.create_network_interface(ip, gateway).await?;

        let node_data_cache = NodeDataCache {
            name: config.name.clone(),
            image: config.image.clone(),
            ip: network_interface.ip().to_string(),
            gateway: network_interface.gateway().to_string(),
            started_at: None,
            standalone: config.standalone,
            requirements: meta.requirements.clone(),
        };

        let node_data = NodeData {
            id,
            name: config.name.clone(),
            image: config.image,
            kernel: meta.kernel,
            expected_status: NodeStatus::Stopped,
            started_at: None,
            network_interface,
            requirements: meta.requirements,
            properties,
            network: config.network,
            firewall_rules: config.rules,
            initialized: false,
            standalone: config.standalone,
            has_pending_update: false,
            restarting: false,
            org_id: config.org_id,
        };

        let node = Node::create(
            self.pal.clone(),
            self.api_config.clone(),
            node_data,
            self.scheduler.tx(),
        )
        .await?;
        self.nodes.write().await.insert(id, RwLock::new(node));
        node_ids.insert(config.name, id);
        self.node_data_cache
            .write()
            .await
            .insert(id, node_data_cache);
        debug!("Node with id `{}` created", id);

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn upgrade(&self, id: Uuid, image: NodeImage) -> commands::Result<()> {
        if image != self.image(id).await? {
            let nodes_lock = self.nodes.read().await;
            let data = nodes_lock
                .get(&id)
                .ok_or_else(|| Error::NodeNotFound(id))?
                .read()
                .await
                .data
                .clone();

            if image.protocol != data.image.protocol {
                command_failed!(Error::Internal(anyhow!(
                    "Cannot upgrade protocol to `{}`",
                    image.protocol
                )));
            }
            if image.node_type != data.image.node_type {
                command_failed!(Error::Internal(anyhow!(
                    "Cannot upgrade node type to `{}`",
                    image.node_type
                )));
            }
            let new_meta =
                Self::fetch_image_data(self.pal.clone(), self.api_config.clone(), &image).await?;
            if data.kernel != new_meta.kernel {
                command_failed!(Error::Internal(anyhow!("Cannot upgrade kernel")));
            }
            if data.requirements.disk_size_gb != new_meta.requirements.disk_size_gb {
                command_failed!(Error::Internal(anyhow!("Cannot upgrade disk requirements")));
            }

            self.check_node_requirements(&new_meta.requirements, &image, Some(&data.requirements))
                .await?;

            let mut node = nodes_lock
                .get(&id)
                .ok_or_else(|| Error::NodeNotFound(id))?
                .write()
                .await;

            node.upgrade(&image).await?;

            let mut cache = self.node_data_cache.write().await;
            cache.entry(id).and_modify(|data| {
                data.image = image;
            });
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn delete(&self, id: Uuid) -> commands::Result<()> {
        let name = {
            let nodes_lock = self.nodes.read().await;
            let mut node = nodes_lock
                .get(&id)
                .ok_or_else(|| Error::NodeNotFound(id))?
                .write()
                .await;
            node.delete().await?;
            node.data.name.clone()
        };
        self.nodes.write().await.remove(&id);
        self.node_ids.write().await.remove(&name);
        self.node_data_cache.write().await.remove(&id);
        debug!("Node deleted");
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn start(&self, id: Uuid, reload_plugin: bool) -> commands::Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut node = nodes_lock
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        if reload_plugin {
            node.reload_plugin()
                .await
                .map_err(|err| BabelError::Internal { err })
                .map_err(into_internal)?;
        }
        if NodeStatus::Running != node.expected_status() {
            node.start().await?;
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn stop(&self, id: Uuid, force: bool) -> commands::Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut node = nodes_lock
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        if NodeStatus::Stopped != node.expected_status() || force {
            node.stop(force).await?;
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn restart(&self, id: Uuid, force: bool) -> commands::Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut node = nodes_lock
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.restart(force).await?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn update(
        &self,
        id: Uuid,
        rules: Vec<firewall::Rule>,
        org_id: String,
    ) -> commands::Result<()> {
        check_user_firewall_rules(&rules)?;
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.update(rules, org_id).await
    }

    #[instrument(skip(self))]
    pub async fn status(&self, id: Uuid) -> Result<NodeStatus> {
        let nodes = self.nodes.read().await;
        let node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .read()
            .await;
        Ok(node.status().await)
    }

    #[instrument(skip(self))]
    async fn expected_status(&self, id: Uuid) -> Result<NodeStatus> {
        let nodes = self.nodes.read().await;
        let node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .read()
            .await;
        Ok(node.expected_status())
    }

    /// Recovery helps nodes to achieve expected state,
    /// in case of actual state and expected state do not match.
    ///
    /// There are several types of recovery:
    /// - Node is stopped, but should be running - in that case we try to start the node
    /// - Node is started, but should be stopped - stop the node
    /// - Node is created, but data files are corrupted - recreate the node
    #[instrument(skip(self))]
    pub async fn recover(&self) {
        let nodes_lock = self.nodes.read().await;
        for (id, node_lock) in nodes_lock.iter() {
            if let Ok(mut node) = node_lock.try_write() {
                if node.status().await == NodeStatus::Failed
                    && node.expected_status() != NodeStatus::Failed
                {
                    if let Err(e) = node.recover().await {
                        error!("node `{id}` recovery failed with: {e:#}");
                    }
                }
            }
        }
    }

    #[instrument(skip(self))]
    pub async fn jobs(&self, id: Uuid) -> Result<JobsInfo> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.babel_engine.get_jobs().await
    }

    #[instrument(skip(self))]
    pub async fn job_info(&self, id: Uuid, job_name: &str) -> Result<JobInfo> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.babel_engine.job_info(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn start_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.babel_engine.start_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn stop_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.babel_engine.stop_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn cleanup_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.babel_engine.cleanup_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn logs(&self, id: Uuid) -> Result<Vec<String>> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.babel_engine.get_logs().await
    }

    #[instrument(skip(self))]
    pub async fn babel_logs(&self, id: Uuid, max_lines: u32) -> Result<Vec<String>> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        node.babel_engine.get_babel_logs(max_lines).await
    }

    #[instrument(skip(self))]
    pub async fn metrics(&self, id: Uuid) -> Result<node_metrics::Metric> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;

        node_metrics::collect_metric(&mut node.babel_engine)
            .await
            .ok_or(anyhow!("metrics not available"))
    }

    #[instrument(skip(self))]
    pub async fn capabilities(&self, id: Uuid) -> Result<Vec<String>> {
        let nodes = self.nodes.read().await;
        let node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .write()
            .await;
        Ok(node.babel_engine.capabilities().clone())
    }

    #[instrument(skip(self))]
    pub async fn call_method(
        &self,
        id: Uuid,
        method: &str,
        param: &str,
        reload_plugin: bool,
    ) -> eyre::Result<String, BabelError> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))
            .map_err(|err| BabelError::Internal { err: err.into() })?
            .write()
            .await;

        if reload_plugin {
            node.reload_plugin()
                .await
                .map_err(|err| BabelError::Internal { err })?;
        }
        if !node.babel_engine.has_capability(method) {
            Err(BabelError::MethodNotFound)
        } else {
            node.babel_engine
                .call_method(method, param)
                .await
                .map_err(|err| BabelError::Plugin { err })
        }
    }

    /// Check if we have enough resources on the host to create/upgrade the node
    ///
    /// Optional tolerance parameter is useful if we want to allow some overbooking.
    /// It also can be used if we want to upgrade the node that exists.
    #[instrument(skip(self))]
    async fn check_node_requirements(
        &self,
        requirements: &Requirements,
        image: &NodeImage,
        tolerance: Option<&Requirements>,
    ) -> commands::Result<()> {
        let mut available = self
            .pal
            .available_resources(&self.nodes_data_cache().await)?;
        debug!("Available resources {available:?}");

        if let Some(tol) = tolerance {
            available.disk_size_gb += tol.disk_size_gb;
            available.mem_size_mb += tol.mem_size_mb;
            available.vcpu_count += tol.vcpu_count;
        }

        // take into account additional copy of os.img made while creating vm
        let os_image_size_gb =
            blockchain::get_image_download_folder_path(self.pal.bv_root(), image)
                .join(ROOT_FS_FILE)
                .metadata()
                .with_context(|| format!("can't check '{ROOT_FS_FILE}' size for {image}"))?
                .len()
                / 1_000_000_000;
        if requirements.disk_size_gb + os_image_size_gb > available.disk_size_gb {
            command_failed!(Error::Internal(anyhow!(
                "Not enough disk space to allocate for the node: required={}+{}, available={}",
                requirements.disk_size_gb,
                os_image_size_gb,
                available.disk_size_gb
            )));
        }
        if requirements.mem_size_mb > available.mem_size_mb {
            command_failed!(Error::Internal(anyhow!(
                "Not enough memory to allocate for the node"
            )));
        }
        if requirements.vcpu_count > available.vcpu_count {
            command_failed!(Error::Internal(anyhow!(
                "Not enough vcpu to allocate for the node"
            )));
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn image(&self, id: Uuid) -> commands::Result<NodeImage> {
        let nodes = self.nodes.read().await;
        let node = nodes
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound(id))?
            .read()
            .await;
        Ok(node.data.image.clone())
    }

    pub async fn node_data_cache(&self, id: Uuid) -> commands::Result<NodeDataCache> {
        let cache = self
            .node_data_cache
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::NodeNotFound(id))?;

        Ok(cache)
    }

    pub async fn nodes_data_cache(&self) -> NodesDataCache {
        self.node_data_cache
            .read()
            .await
            .iter()
            .map(|(id, node)| (*id, node.clone()))
            .collect()
    }

    pub fn pal(&self) -> &P {
        &self.pal
    }

    async fn load_nodes(
        pal: Arc<P>,
        api_config: SharedConfig,
        registry_dir: &Path,
        tx: mpsc::Sender<scheduler::Action>,
    ) -> Result<(
        HashMap<Uuid, RwLock<Node<P>>>,
        HashMap<String, Uuid>,
        HashMap<Uuid, NodeDataCache>,
    )> {
        info!("Reading nodes config dir: {}", registry_dir.display());
        let mut nodes = HashMap::new();
        let mut node_ids = HashMap::new();
        let mut node_data_cache = HashMap::new();
        let mut vm_processes = vec![];
        let mut dir = read_dir(registry_dir)
            .await
            .context("failed to read nodes registry dir")?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .context("failed to read nodes registry entry")?
        {
            let path = entry.path();
            if path
                .extension()
                .and_then(|v| if "json" == v { Some(()) } else { None })
                .is_none()
            {
                continue; // ignore other files in registry dir
            }
            match NodeData::load(&path)
                .and_then(|data| async {
                    Node::attach(pal.clone(), api_config.clone(), data, tx.clone()).await
                })
                .await
            {
                Ok(node) => {
                    // remove VM pid from list of all discovered VM pids
                    // in the end of load this list should be empty
                    if node.status().await == NodeStatus::Running {
                        vm_processes.push(pal.get_vm_pid(node.id())?);
                    }
                    // insert node and its info into internal data structures
                    let id = node.id();
                    let name = node.data.name.clone();
                    node_ids.insert(name.clone(), id);
                    node_data_cache.insert(
                        id,
                        NodeDataCache {
                            name,
                            ip: node.data.network_interface.ip().to_string(),
                            gateway: node.data.network_interface.gateway().to_string(),
                            image: node.data.image.clone(),
                            started_at: node.data.started_at,
                            standalone: node.data.standalone,
                            requirements: node.data.requirements.clone(),
                        },
                    );
                    nodes.insert(id, RwLock::new(node));
                }
                Err(e) => {
                    // blockvisord should not bail on problems with individual node files.
                    // It should log error though.
                    error!(
                        "Failed to load node from file `{}`: {:#}",
                        path.display(),
                        e
                    );
                }
            };
        }
        // check if we run some unmanaged FC processes on the host
        for pid in pal.get_vm_pids()? {
            if !vm_processes.contains(&pid) {
                error!("VM with pid {pid} is not managed by BV");
            }
        }

        Ok((nodes, node_ids, node_data_cache))
    }

    /// Create and return the next network interface using machine index
    async fn create_network_interface(
        &self,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<P::NetInterface> {
        let machine_index = {
            let mut data = self.state.write().await;
            data.machine_index += 1;
            data.machine_index
        };
        let iface = self
            .pal
            .create_net_interface(machine_index, ip, gateway, &self.api_config)
            .await
            .context(format!("failed to create VM bridge bv{}", machine_index))?;
        let res = self.state.read().await.save(&self.registry_path).await;
        if res.is_err() {
            if let Err(err) = iface.delete().await {
                error!("Can't delete network interface after unsuccessful node create: {err:#}");
            }
            res?
        }
        Ok(iface)
    }

    #[instrument(skip(pal, api_config))]
    pub async fn fetch_image_data(
        pal: Arc<P>,
        api_config: SharedConfig,
        image: &NodeImage,
    ) -> Result<BlockchainMetadata> {
        let bv_root = pal.bv_root();
        let folder = blockchain::get_image_download_folder_path(bv_root, image);
        let rhai_path = folder.join(BABEL_PLUGIN_NAME);

        let script = if !blockchain::is_image_cache_valid(bv_root, image)
            .await
            .with_context(|| format!("Failed to check image cache: `{image:?}`"))?
        {
            let mut blockchain_service = BlockchainService::new(
                pal.create_api_service_connector(&api_config),
                bv_root.to_path_buf(),
            )
            .await
            .with_context(|| "cannot connect to blockchain service")?;
            blockchain_service
                .download_babel_plugin(image)
                .await
                .with_context(|| "cannot download babel plugin")?;
            blockchain_service
                .download_image(image)
                .await
                .with_context(|| "cannot download image")?;
            fs::read_to_string(rhai_path).await.map_err(into_internal)?
        } else {
            fs::read_to_string(rhai_path).await.map_err(into_internal)?
        };
        let meta = rhai_plugin::read_metadata(&script)?;
        if !kernel::is_kernel_cache_valid(bv_root, &meta.kernel)
            .await
            .with_context(|| format!("Failed to check kernel cache: `{}`", meta.kernel))?
        {
            kernel::download_kernel(
                bv_root,
                pal.create_api_service_connector(&api_config),
                &meta.kernel,
            )
            .await
            .with_context(|| "cannot download kernel")?;
        }

        info!("Reading blockchain requirements: {:?}", &meta.requirements);
        Ok(meta)
    }
}

fn check_user_firewall_rules(rules: &[firewall::Rule]) -> commands::Result<()> {
    if rules.len() > MAX_SUPPORTED_RULES {
        command_failed!(Error::Internal(anyhow!(
            "Can't configure more than {MAX_SUPPORTED_RULES} rules!"
        )));
    }
    babel_api::metadata::check_firewall_rules(rules)?;
    Ok(())
}

fn name_not_found(name: &str) -> eyre::Error {
    anyhow!("Node with name `{}` not found", name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        node::tests::*,
        pal,
        pal::VmState,
        services::{
            api::{common, pb},
            blockchain::ROOT_FS_FILE,
            kernel::KERNELS_DIR,
        },
        utils,
    };
    use assert_fs::TempDir;
    use bv_tests_utils::start_test_server;
    use bv_utils::cmd::run_cmd;
    use eyre::bail;
    use mockall::*;
    use std::ffi::OsStr;
    use std::str::FromStr;

    mock! {
        pub TestKernelService {}

        #[tonic::async_trait]
        impl pb::kernel_service_server::KernelService for TestKernelService {
            async fn retrieve(&self, request: tonic::Request<pb::KernelServiceRetrieveRequest>
            ) -> Result<tonic::Response<pb::KernelServiceRetrieveResponse>, tonic::Status>;
            async fn list_kernel_versions(&self, request: tonic::Request<pb::KernelServiceListKernelVersionsRequest>,
            ) -> Result<tonic::Response<pb::KernelServiceListKernelVersionsResponse>, tonic::Status>;
        }
    }

    mock! {
        pub TestBlockchainService {}

        #[tonic::async_trait]
        impl pb::blockchain_service_server::BlockchainService for TestBlockchainService {
            async fn get(
                &self,
                request: tonic::Request<pb::BlockchainServiceGetRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceGetResponse>, tonic::Status>;
            async fn get_image(
                &self,
                request: tonic::Request<pb::BlockchainServiceGetImageRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceGetImageResponse>, tonic::Status>;
            async fn get_plugin(
                &self,
                request: tonic::Request<pb::BlockchainServiceGetPluginRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceGetPluginResponse>, tonic::Status>;
            async fn get_requirements(
                &self,
                request: tonic::Request<pb::BlockchainServiceGetRequirementsRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceGetRequirementsResponse>, tonic::Status>;
            async fn list(
                &self,
                request: tonic::Request<pb::BlockchainServiceListRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceListResponse>, tonic::Status>;
            async fn list_image_versions(
                &self,
                request: tonic::Request<pb::BlockchainServiceListImageVersionsRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceListImageVersionsResponse>, tonic::Status>;
            async fn add_node_type(
                &self,
                request: tonic::Request<pb::BlockchainServiceAddNodeTypeRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceAddNodeTypeResponse>, tonic::Status>;
            async fn add_version(
                &self,
                request: tonic::Request<pb::BlockchainServiceAddVersionRequest>,
            ) -> Result<tonic::Response<pb::BlockchainServiceAddVersionResponse>, tonic::Status>;
        }
    }

    struct TestEnv {
        tmp_root: PathBuf,
        test_image: NodeImage,
        _async_panic_checker: utils::tests::AsyncPanicChecker,
    }

    impl TestEnv {
        async fn new() -> Result<Self> {
            let tmp_root = TempDir::new()?.to_path_buf();
            fs::create_dir_all(&tmp_root).await?;

            Ok(Self {
                tmp_root,
                test_image: NodeImage {
                    protocol: "testing".to_string(),
                    node_type: "validator".to_string(),
                    node_version: "1.2.3".to_string(),
                },
                _async_panic_checker: Default::default(),
            })
        }

        fn default_pal(&self) -> MockTestPal {
            let mut pal = MockTestPal::new();
            pal.expect_bv_root()
                .return_const(self.tmp_root.to_path_buf());
            pal.expect_babel_path()
                .return_const(self.tmp_root.join("babel"));
            pal.expect_job_runner_path()
                .return_const(self.tmp_root.join("job_runner"));
            let tmp_root = self.tmp_root.clone();
            pal.expect_build_vm_data_path()
                .returning(move |id| tmp_root.clone().join(format!("vm_data_{id}")));
            pal.expect_create_commands_stream_connector()
                .return_const(EmptyStreamConnector);
            pal.expect_create_api_service_connector()
                .return_const(TestConnector {
                    tmp_root: self.tmp_root.clone(),
                });
            pal.expect_create_recovery_backoff()
                .return_const(DummyBackoff::default());
            pal
        }

        async fn generate_dummy_archive(&self) {
            let mut file_path = self.tmp_root.join(ROOT_FS_FILE).into_os_string();
            fs::write(&file_path, "dummy archive content")
                .await
                .unwrap();
            let archive_file_path = &self.tmp_root.join("blockjoy.gz");
            run_cmd("gzip", [OsStr::new("-kf"), &file_path])
                .await
                .unwrap();
            file_path.push(".gz");
            fs::rename(file_path, archive_file_path).await.unwrap();
        }

        async fn start_test_server(
            &self,
            images: Vec<(NodeImage, Vec<u8>)>,
        ) -> (
            bv_tests_utils::rpc::TestServer,
            mockito::ServerGuard,
            Vec<mockito::Mock>,
        ) {
            self.generate_dummy_archive().await;
            let mut http_server = mockito::Server::new();
            let mut http_mocks = vec![];

            // expect image retrieve and download for all images, but only once
            let mut blockchain = MockTestBlockchainService::new();
            for (test_image, rhai_content) in images {
                let node_image = Some(common::ImageIdentifier {
                    protocol: test_image.protocol.clone(),
                    node_type: common::NodeType::from_str(&test_image.node_type)
                        .unwrap()
                        .into(),
                    node_version: test_image.node_version.clone(),
                });
                let expected_image = node_image.clone();
                let resp = tonic::Response::new(pb::BlockchainServiceGetPluginResponse {
                    plugin: Some(common::RhaiPlugin {
                        identifier: expected_image.clone(),
                        rhai_content,
                    }),
                });
                blockchain
                    .expect_get_plugin()
                    .withf(move |req| req.get_ref().id == expected_image)
                    .return_once(move |_| Ok(resp));
                let url = http_server.url();
                let expected_image = node_image.clone();
                blockchain
                    .expect_get_image()
                    .withf(move |req| req.get_ref().id == expected_image)
                    .return_once(move |_| {
                        Ok(tonic::Response::new(
                            pb::BlockchainServiceGetImageResponse {
                                location: Some(common::ArchiveLocation {
                                    url: format!("{url}/image"),
                                }),
                            },
                        ))
                    });
                http_mocks.push(
                    http_server
                        .mock("GET", "/image")
                        .with_body_from_file(&*self.tmp_root.join("blockjoy.gz").to_string_lossy())
                        .create(),
                );
            }

            // expect kernel retrieve and download,but only once
            let url = http_server.url();
            let mut kernels = MockTestKernelService::new();
            kernels
                .expect_retrieve()
                .withf(|req| {
                    req.get_ref().id
                        == Some(pb::KernelIdentifier {
                            version: TEST_KERNEL.to_string(),
                        })
                })
                .return_once(move |_| {
                    Ok(tonic::Response::new(pb::KernelServiceRetrieveResponse {
                        location: Some(common::ArchiveLocation {
                            url: format!("{url}/kernel"),
                        }),
                    }))
                });
            http_mocks.push(
                http_server
                    .mock("GET", "/kernel")
                    .with_body_from_file(&*self.tmp_root.join("blockjoy.gz").to_string_lossy())
                    .create(),
            );

            (
                start_test_server!(
                    &self.tmp_root,
                    pb::kernel_service_server::KernelServiceServer::new(kernels),
                    pb::blockchain_service_server::BlockchainServiceServer::new(blockchain)
                ),
                http_server,
                http_mocks,
            )
        }
    }

    const TEST_NODE_REQUIREMENTS: Requirements = Requirements {
        vcpu_count: 1,
        mem_size_mb: 2048,
        disk_size_gb: 1,
    };

    fn available_test_resources(
        _nodes_data_cache: &NodesDataCache,
    ) -> Result<pal::AvailableResources> {
        Ok(TEST_NODE_REQUIREMENTS)
    }

    fn add_create_node_expectations(
        pal: &mut MockTestPal,
        expected_index: u32,
        id: Uuid,
        config: NodeConfig,
        vm_mock: MockTestVM,
    ) {
        let expected_ip = config.ip.clone();
        let expected_gateway = config.gateway.clone();
        pal.expect_available_resources()
            .withf(move |req| expected_index - 1 == req.len() as u32)
            .once()
            .returning(available_test_resources);
        pal.expect_create_net_interface()
            .withf(move |index, ip, gateway, _| {
                *index == expected_index
                    && ip.to_string() == expected_ip
                    && gateway.to_string() == expected_gateway
            })
            .return_once(|index, ip, gateway, _config| {
                Ok(DummyNet {
                    name: format!("bv{index}"),
                    ip,
                    gateway,
                    remaster_error: None,
                    delete_error: Some("net delete error".to_string()),
                })
            });
        pal.expect_create_vm()
            .with(predicate::eq(expected_node_data(
                expected_index,
                id,
                config,
                None,
            )))
            .return_once(move |_| Ok(vm_mock));
        pal.expect_create_node_connection()
            .with(predicate::eq(id))
            .return_once(dummy_connection_mock);
    }

    fn add_create_node_fail_vm_expectations(
        pal: &mut MockTestPal,
        expected_index: u32,
        id: Uuid,
        config: NodeConfig,
    ) {
        let expected_ip = config.ip.clone();
        let expected_gateway = config.gateway.clone();
        pal.expect_available_resources()
            .withf(move |req| expected_index - 1 == req.len() as u32)
            .once()
            .returning(|_requirements| bail!("failed to check available resources"));
        pal.expect_available_resources()
            .withf(move |req| expected_index - 1 == req.len() as u32)
            .once()
            .returning(|_requirements| {
                Ok(pal::AvailableResources {
                    vcpu_count: 1,
                    mem_size_mb: 1024,
                    disk_size_gb: 1,
                })
            });
        pal.expect_available_resources()
            .withf(move |req| expected_index - 1 == req.len() as u32)
            .returning(available_test_resources);
        pal.expect_create_net_interface()
            .withf(move |index, ip, gateway, _| {
                *index == expected_index
                    && ip.to_string() == expected_ip
                    && gateway.to_string() == expected_gateway
            })
            .return_once(|index, ip, gateway, _config| {
                Ok(DummyNet {
                    name: format!("bv{index}"),
                    ip,
                    gateway,
                    remaster_error: None,
                    delete_error: Some("net delete error".to_string()),
                })
            });
        pal.expect_create_vm()
            .with(predicate::eq(expected_node_data(
                expected_index,
                id,
                config,
                None,
            )))
            .return_once(|_| bail!("failed to create vm"));
    }

    fn expected_node_data(
        expected_index: u32,
        id: Uuid,
        config: NodeConfig,
        image: Option<NodeImage>,
    ) -> NodeData<DummyNet> {
        NodeData {
            id,
            name: config.name,
            expected_status: NodeStatus::Stopped,
            started_at: None,
            initialized: false,
            has_pending_update: false,
            image: image.unwrap_or(config.image),
            kernel: TEST_KERNEL.to_string(),
            network_interface: DummyNet {
                name: format!("bv{expected_index}"),
                ip: IpAddr::from_str(&config.ip).unwrap(),
                gateway: IpAddr::from_str(&config.gateway).unwrap(),
                remaster_error: None,
                delete_error: Some("net delete error".to_string()),
            },
            requirements: TEST_NODE_REQUIREMENTS,
            firewall_rules: config.rules,
            properties: config
                .properties
                .into_iter()
                .chain([("NETWORK".to_string(), config.network.clone())])
                .collect(),
            network: config.network,
            standalone: config.standalone,
            restarting: false,
            org_id: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_create_node_and_delete() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());

        let first_node_id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap();
        let first_node_config = NodeConfig {
            name: "first node name".to_string(),
            image: test_env.test_image.clone(),
            ip: "192.168.0.7".to_string(),
            gateway: "192.168.0.1".to_string(),
            rules: vec![],
            properties: Default::default(),
            network: "test".to_string(),
            standalone: true,
            org_id: Default::default(),
        };
        let mut vm_mock = MockTestVM::new();
        vm_mock.expect_state().once().return_const(VmState::SHUTOFF);
        vm_mock
            .expect_delete()
            .once()
            .returning(|| bail!("delete VM failed"));
        add_create_node_expectations(
            &mut pal,
            1,
            first_node_id,
            first_node_config.clone(),
            vm_mock,
        );

        let second_node_id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047531").unwrap();
        let second_node_config = NodeConfig {
            name: "second node name".to_string(),
            image: test_env.test_image.clone(),
            ip: "192.168.0.8".to_string(),
            gateway: "192.168.0.1".to_string(),
            rules: vec![],
            properties: Default::default(),
            network: "test".to_string(),
            standalone: false,
            org_id: Default::default(),
        };
        let mut vm_mock = MockTestVM::new();
        vm_mock.expect_state().once().return_const(VmState::SHUTOFF);
        vm_mock.expect_delete().once().returning(|| Ok(()));
        add_create_node_expectations(
            &mut pal,
            2,
            second_node_id,
            second_node_config.clone(),
            vm_mock,
        );

        let failed_node_id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047532").unwrap();
        let failed_node_config = NodeConfig {
            name: "failed node name".to_string(),
            image: test_env.test_image.clone(),
            ip: "192.168.0.9".to_string(),
            gateway: "192.168.0.1".to_string(),
            rules: vec![],
            properties: Default::default(),
            network: "test".to_string(),
            standalone: false,
            org_id: Default::default(),
        };
        add_create_node_fail_vm_expectations(
            &mut pal,
            3,
            failed_node_id,
            failed_node_config.clone(),
        );
        // expectations for create_net failed
        let expected_ip = failed_node_config.ip.clone();
        let expected_gateway = failed_node_config.gateway.clone();
        pal.expect_create_net_interface()
            .withf(move |index, ip, gateway, _| {
                *index == 4
                    && ip.to_string() == expected_ip
                    && gateway.to_string() == expected_gateway
            })
            .return_once(|_, _, _, _| bail!("failed to create net iface"));

        let nodes = NodesManager::load(pal, config).await?;
        assert!(nodes.nodes_list().await.is_empty());

        let (test_server, _http_server, http_mocks) = test_env
            .start_test_server(vec![(
                test_env.test_image.clone(),
                include_bytes!("../tests/babel.rhai").to_vec(),
            )])
            .await;

        nodes
            .create(first_node_id, first_node_config.clone())
            .await?;
        nodes
            .create(second_node_id, second_node_config.clone())
            .await?;
        nodes
            .create(second_node_id, second_node_config.clone())
            .await?;
        assert_eq!(
            "BV internal error: failed to check available resources",
            nodes
                .create(failed_node_id, failed_node_config.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: Not enough memory to allocate for the node",
            nodes
                .create(failed_node_id, failed_node_config.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: failed to create vm",
            nodes
                .create(failed_node_id, failed_node_config.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: failed to create VM bridge bv4: failed to create net iface",
            nodes
                .create(failed_node_id, failed_node_config)
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: Node with name `first node name` exists",
            nodes
                .create(failed_node_id, first_node_config.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: Node with ip address `192.168.0.7` exists",
            nodes
                .create(
                    failed_node_id,
                    NodeConfig {
                        name: "node name".to_string(),
                        image: test_env.test_image.clone(),
                        ip: "192.168.0.7".to_string(),
                        gateway: "192.168.0.1".to_string(),
                        rules: vec![],
                        properties: Default::default(),
                        network: "test".to_string(),
                        standalone: true,
                        org_id: Default::default(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: invalid ip `invalid`: invalid IP address syntax",
            nodes
                .create(
                    failed_node_id,
                    NodeConfig {
                        name: "node name".to_string(),
                        image: test_env.test_image.clone(),
                        ip: "invalid".to_string(),
                        gateway: "192.168.0.1".to_string(),
                        rules: vec![],
                        properties: Default::default(),
                        network: "test".to_string(),
                        standalone: true,
                        org_id: Default::default(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: invalid gateway `invalid`: invalid IP address syntax",
            nodes
                .create(
                    failed_node_id,
                    NodeConfig {
                        name: "node name".to_string(),
                        image: test_env.test_image.clone(),
                        ip: "192.168.0.9".to_string(),
                        gateway: "invalid".to_string(),
                        rules: vec![],
                        properties: Default::default(),
                        network: "test".to_string(),
                        standalone: true,
                        org_id: Default::default(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        let rules = (0..129)
            .map(|n| firewall::Rule {
                name: format!("rule name {n}"),
                action: firewall::Action::Allow,
                direction: firewall::Direction::Out,
                protocol: None,
                ips: None,
                ports: vec![],
            })
            .collect::<Vec<_>>();
        assert_eq!(
            "BV internal error: Can't configure more than 128 rules!",
            nodes
                .create(
                    failed_node_id,
                    NodeConfig {
                        name: "node name".to_string(),
                        image: test_env.test_image.clone(),
                        ip: "192.168.0.9".to_string(),
                        gateway: "192.168.0.1".to_string(),
                        rules,
                        properties: Default::default(),
                        network: "test".to_string(),
                        standalone: true,
                        org_id: Default::default(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: fetch image data failed: cannot download babel plugin: Invalid NodeType invalid",
            nodes
                .create(
                    failed_node_id,
                    NodeConfig {
                        name: "node name".to_string(),
                        image: NodeImage {
                            protocol: "testing".to_string(),
                            node_type: "invalid".to_string(),
                            node_version: "1.2.3".to_string(),
                        },
                        ip: "192.168.0.9".to_string(),
                        gateway: "192.168.0.1".to_string(),
                        rules: vec![],
                        properties: Default::default(),
                        network: "test".to_string(),
                        standalone: true,org_id: Default::default(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: invalid network name 'invalid'",
            nodes
                .create(
                    failed_node_id,
                    NodeConfig {
                        name: "node name".to_string(),
                        image: test_env.test_image.clone(),
                        ip: "192.168.0.9".to_string(),
                        gateway: "192.168.0.1".to_string(),
                        rules: vec![],
                        properties: Default::default(),
                        network: "invalid".to_string(),
                        standalone: true,
                        org_id: Default::default(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );

        assert_eq!(2, nodes.nodes_list().await.len());
        assert_eq!(
            second_node_id,
            nodes.node_id_for_name(&second_node_config.name).await?
        );
        assert_eq!(NodeStatus::Stopped, nodes.status(first_node_id).await?);
        assert_eq!(NodeStatus::Stopped, nodes.status(second_node_id).await?);
        assert_eq!(
            NodeStatus::Stopped,
            nodes.expected_status(first_node_id).await?
        );
        assert_eq!(
            NodeDataCache {
                name: first_node_config.name,
                image: first_node_config.image,
                ip: first_node_config.ip,
                gateway: first_node_config.gateway,
                started_at: None,
                standalone: first_node_config.standalone,
                requirements: TEST_NODE_REQUIREMENTS,
            },
            nodes.node_data_cache(first_node_id).await?
        );

        assert_eq!(
            "BV internal error: delete VM failed",
            nodes.delete(first_node_id).await.unwrap_err().to_string()
        );

        assert_eq!(
            "BV internal error: net delete error",
            nodes.delete(second_node_id).await.unwrap_err().to_string()
        );

        for mock in http_mocks {
            mock.assert();
        }
        test_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_load() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());

        let node_data = NodeData {
            id: Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap(),
            name: "first node".to_string(),
            expected_status: NodeStatus::Stopped,
            started_at: None,
            initialized: false,
            has_pending_update: false,
            image: test_env.test_image.clone(),
            kernel: TEST_KERNEL.to_string(),
            network_interface: DummyNet {
                name: "bv1".to_string(),
                ip: IpAddr::from_str("192.168.0.9").unwrap(),
                gateway: IpAddr::from_str("192.168.0.1").unwrap(),
                remaster_error: None,
                delete_error: None,
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 1024,
                disk_size_gb: 1,
            },
            firewall_rules: vec![],
            properties: Default::default(),
            network: "test".to_string(),
            standalone: false,
            restarting: false,
            org_id: Default::default(),
        };
        fs::create_dir_all(pal.build_vm_data_path(node_data.id)).await?;

        let nodes = NodesManager::load(pal, config).await?;
        assert!(nodes.nodes_list().await.is_empty());

        let mut invalid_node_data = node_data.clone();
        invalid_node_data.id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047531").unwrap();
        let registry_dir = build_registry_dir(&test_env.tmp_root);
        fs::create_dir_all(&registry_dir).await?;
        node_data.save(&registry_dir).await?;
        invalid_node_data.save(&registry_dir).await?;
        fs::copy(
            testing_babel_path_absolute(),
            registry_dir.join(format!("{}.rhai", node_data.id)),
        )
        .await?;
        fs::copy(
            testing_babel_path_absolute(),
            registry_dir.join(format!("{}.rhai", invalid_node_data.id)),
        )
        .await?;
        fs::write(
            registry_dir.join("4931bafa-92d9-4521-9fc6-a77eee047533.json"),
            "invalid node data",
        )
        .await?;

        let mut pal = test_env.default_pal();
        pal.expect_create_node_connection()
            .with(predicate::eq(node_data.id))
            .returning(dummy_connection_mock);
        pal.expect_attach_vm()
            .with(predicate::eq(node_data.clone()))
            .returning(|_| {
                let mut vm = MockTestVM::new();
                vm.expect_state().return_const(VmState::SHUTOFF);
                Ok(vm)
            });
        pal.expect_create_node_connection()
            .with(predicate::eq(invalid_node_data.id))
            .returning(dummy_connection_mock);
        pal.expect_attach_vm()
            .with(predicate::eq(invalid_node_data.clone()))
            .returning(|_| {
                bail!("failed to attach");
            });
        pal.expect_get_vm_pids()
            .return_once(|| Ok(Default::default()));
        let config = default_config(test_env.tmp_root.clone());
        let nodes = NodesManager::load(pal, config).await?;
        assert_eq!(1, nodes.nodes_list().await.len());
        assert_eq!(
            "first node",
            nodes.node_data_cache(node_data.id).await?.name
        );
        assert_eq!(
            NodeStatus::Stopped,
            nodes.expected_status(node_data.id).await?
        );
        assert_eq!(node_data.id, nodes.node_id_for_name("first node").await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_upgrade_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());

        let node_id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap();
        let node_config = NodeConfig {
            name: "node name".to_string(),
            image: test_env.test_image.clone(),
            ip: "192.168.0.7".to_string(),
            gateway: "192.168.0.1".to_string(),
            rules: vec![],
            properties: Default::default(),
            network: "test".to_string(),
            standalone: true,
            org_id: Default::default(),
        };
        let new_image = NodeImage {
            protocol: "testing".to_string(),
            node_type: "validator".to_string(),
            node_version: "3.2.1".to_string(),
        };
        let invalid_kernel_image = NodeImage {
            protocol: "testing".to_string(),
            node_type: "validator".to_string(),
            node_version: "3.4.5".to_string(),
        };
        let invalid_disk_size_image = NodeImage {
            protocol: "testing".to_string(),
            node_type: "validator".to_string(),
            node_version: "3.4.6".to_string(),
        };
        let cpu_devourer_image = NodeImage {
            protocol: "testing".to_string(),
            node_type: "validator".to_string(),
            node_version: "3.4.7".to_string(),
        };
        let mut vm_mock = MockTestVM::new();
        vm_mock.expect_state().once().return_const(VmState::SHUTOFF);
        vm_mock.expect_release().return_once(|| Ok(()));
        add_create_node_expectations(&mut pal, 1, node_id, node_config.clone(), vm_mock);
        pal.expect_available_resources()
            .withf(move |req| 1 == req.len() as u32)
            .once()
            .returning(|_requirements| bail!("failed to get available resources"));
        pal.expect_available_resources()
            .withf(move |req| 1 == req.len() as u32)
            .times(2)
            .returning(available_test_resources);
        pal.expect_attach_vm()
            .with(predicate::eq(expected_node_data(
                1,
                node_id,
                node_config.clone(),
                Some(new_image.clone()),
            )))
            .return_once(|_| Ok(MockTestVM::new()));

        let nodes = NodesManager::load(pal, config).await?;

        let (test_server, _http_server, http_mocks) = test_env
            .start_test_server(vec![
                (
                    test_env.test_image.clone(),
                    include_bytes!("../tests/babel.rhai").to_vec(),
                ),
                (
                    new_image.clone(),
                    format!("{} kernel: \"{}\", requirements: #{{ vcpu_count: {}, mem_size_mb: {}, disk_size_gb: {}}}}};",
                            UPGRADED_IMAGE_RHAI_TEMPLATE, TEST_KERNEL, 1, 2048, 1).into_bytes(),
                ),
                (
                    invalid_kernel_image.clone(),
                    format!("{} kernel: \"{}\", requirements: #{{ vcpu_count: {}, mem_size_mb: {}, disk_size_gb: {}}}}};",
                            UPGRADED_IMAGE_RHAI_TEMPLATE, "5.10.175", 1, 2048, 1).into_bytes(),
                ),
                (
                    invalid_disk_size_image.clone(),
                    format!("{} kernel: \"{}\", requirements: #{{ vcpu_count: {}, mem_size_mb: {}, disk_size_gb: {}}}}};",
                            UPGRADED_IMAGE_RHAI_TEMPLATE, TEST_KERNEL, 1, 2048, 2).into_bytes(),
                ),
                (
                    cpu_devourer_image.clone(),
                    CPU_DEVOURER_IMAGE_RHAI.to_owned().into_bytes(),
                ),
            ])
            .await;

        nodes.create(node_id, node_config.clone()).await?;
        assert_eq!(
            NodeDataCache {
                name: node_config.name.clone(),
                image: node_config.image.clone(),
                ip: node_config.ip.clone(),
                gateway: node_config.gateway.clone(),
                started_at: None,
                standalone: node_config.standalone,
                requirements: TEST_NODE_REQUIREMENTS,
            },
            nodes.node_data_cache(node_id).await?
        );
        {
            let mut nodes_list = nodes.nodes.write().await;
            let mut node = nodes_list.get_mut(&node_id).unwrap().write().await;
            node.data.initialized = true;
            assert!(node.babel_engine.has_capability("info"));
        }
        assert_eq!(
            "BV internal error: failed to get available resources",
            nodes
                .upgrade(node_id, new_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        nodes.upgrade(node_id, new_image.clone()).await?;
        let not_found_id = Uuid::new_v4();
        assert_eq!(
            format!("Node with {not_found_id} not found"),
            nodes
                .upgrade(not_found_id, new_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            NodeDataCache {
                name: node_config.name,
                image: new_image.clone(),
                ip: node_config.ip,
                gateway: node_config.gateway,
                started_at: None,
                standalone: node_config.standalone,
                requirements: TEST_NODE_REQUIREMENTS,
            },
            nodes.node_data_cache(node_id).await?
        );
        {
            let mut nodes_list = nodes.nodes.write().await;
            let mut node = nodes_list.get_mut(&node_id).unwrap().write().await;
            assert!(!node.data.initialized);
            assert!(!node.babel_engine.has_capability("info"));
        }
        fs::create_dir_all(
            test_env
                .tmp_root
                .join(BV_VAR_PATH)
                .join(KERNELS_DIR)
                .join("5.10.175"),
        )
        .await?;
        fs::copy(
            kernel::get_kernel_path(&test_env.tmp_root, TEST_KERNEL),
            kernel::get_kernel_path(&test_env.tmp_root, "5.10.175"),
        )
        .await?;
        assert_eq!(
            "BV internal error: Cannot upgrade kernel",
            nodes
                .upgrade(node_id, invalid_kernel_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: Cannot upgrade disk requirements",
            nodes
                .upgrade(node_id, invalid_disk_size_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: Not enough vcpu to allocate for the node",
            nodes
                .upgrade(node_id, cpu_devourer_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: Cannot upgrade protocol to `differen_chain`",
            nodes
                .upgrade(
                    node_id,
                    NodeImage {
                        protocol: "differen_chain".to_string(),
                        node_type: "validator".to_string(),
                        node_version: "1.2.3".to_string(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: Cannot upgrade node type to `node`",
            nodes
                .upgrade(
                    node_id,
                    NodeImage {
                        protocol: "testing".to_string(),
                        node_type: "node".to_string(),
                        node_version: "1.2.3".to_string(),
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );

        for mock in http_mocks {
            mock.assert();
        }
        test_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_recovery() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let node_id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap();
        let node_config = NodeConfig {
            name: "node name".to_string(),
            image: test_env.test_image.clone(),
            ip: "192.168.0.7".to_string(),
            gateway: "192.168.0.1".to_string(),
            rules: vec![],
            properties: Default::default(),
            network: "test".to_string(),
            standalone: true,
            org_id: Default::default(),
        };

        pal.expect_available_resources()
            .withf(move |req| req.is_empty())
            .once()
            .returning(available_test_resources);
        pal.expect_create_net_interface()
            .return_once(|index, ip, gateway, _config| {
                Ok(DummyNet {
                    name: format!("bv{index}"),
                    ip,
                    gateway,
                    remaster_error: Some("remaster failed".to_string()),
                    delete_error: None,
                })
            });
        pal.expect_create_vm().return_once(|_| {
            let mut mock = MockTestVM::new();
            let mut seq = Sequence::new();
            mock.expect_state()
                .times(7)
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            mock.expect_state()
                .times(3)
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            Ok(mock)
        });
        pal.expect_create_node_connection().return_once(|_| {
            let mut mock = MockTestNodeConnection::new();
            let mut seq = Sequence::new();
            // broken connection
            mock.expect_is_closed()
                .once()
                .in_sequence(&mut seq)
                .returning(|| false);
            mock.expect_is_broken()
                .once()
                .in_sequence(&mut seq)
                .returning(|| true);
            mock.expect_test()
                .once()
                .in_sequence(&mut seq)
                .returning(|| Ok(()));
            mock.expect_is_closed()
                .once()
                .in_sequence(&mut seq)
                .returning(|| false);
            // just running node
            mock.expect_is_closed()
                .once()
                .in_sequence(&mut seq)
                .returning(|| false);
            mock.expect_is_broken()
                .once()
                .in_sequence(&mut seq)
                .returning(|| false);
            mock.expect_engine_socket_path()
                .return_const(Default::default());
            mock
        });

        let mut sut = RecoverySut {
            node_id,
            nodes: NodesManager::load(pal, config).await?,
        };

        let (test_server, _http_server, http_mocks) = test_env
            .start_test_server(vec![(
                test_env.test_image.clone(),
                include_bytes!("../tests/babel.rhai").to_vec(),
            )])
            .await;
        sut.nodes.create(node_id, node_config.clone()).await?;
        // no recovery needed - node is expected to be stopped
        sut.nodes.recover().await;

        // no recovery for permanently failed node
        sut.on_node(|node| node.data.expected_status = NodeStatus::Failed)
            .await;
        sut.nodes.recover().await;

        // recovery of node that is expected to be running, but it is not
        sut.on_node(|node| node.data.expected_status = NodeStatus::Running)
            .await;
        sut.nodes.recover().await;

        // recovery of node that is expected to be stopped, but it is not
        sut.on_node(|node| node.data.expected_status = NodeStatus::Stopped)
            .await;
        sut.nodes.recover().await;

        // node connection recovery
        sut.on_node(|node| {
            node.data.expected_status = NodeStatus::Running;
            node.data.initialized = true;
            node.post_recovery();
        })
        .await;
        sut.nodes.recover().await;

        // no recovery needed - node is expected to be running
        sut.on_node(|node| {
            node.data.expected_status = NodeStatus::Running;
            node.data.initialized = true;
        })
        .await;
        sut.nodes.recover().await;

        for mock in http_mocks {
            mock.assert();
        }
        test_server.assert().await;

        Ok(())
    }

    struct RecoverySut {
        node_id: Uuid,
        nodes: NodesManager<MockTestPal>,
    }

    impl RecoverySut {
        async fn on_node(&mut self, call_on_node: impl FnOnce(&mut Node<MockTestPal>)) {
            call_on_node(
                &mut *self
                    .nodes
                    .nodes
                    .write()
                    .await
                    .get_mut(&self.node_id)
                    .unwrap()
                    .write()
                    .await,
            );
        }
    }

    const UPGRADED_IMAGE_RHAI_TEMPLATE: &str = r#"
const METADATA = #{
    min_babel_version: "0.0.9",
    node_version: "1.15.9",
    protocol: "testing",
    node_type: "validator",
    nets: #{
        test: #{
            url: "https://testnet-api.helium.wtf/v1/",
            net_type: "test",
        },
    },
    babel_config: #{
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 512,
        ramdisks: []
    },
    firewall: #{
        enabled: true,
        default_in: "deny",
        default_out: "allow",
        rules: [],
    },
"#;

    const CPU_DEVOURER_IMAGE_RHAI: &str = r#"
const METADATA = #{
    min_babel_version: "0.0.9",
    kernel: "5.10.174-build.1+fc.ufw",
    node_version: "1.15.9",
    protocol: "huge_blockchain",
    node_type: "validator",
    requirements: #{
        vcpu_count: 2048,
        mem_size_mb: 2048,
        disk_size_gb: 1,
    },
    nets: #{
        test: #{
            url: "https://testnet-api.helium.wtf/v1/",
            net_type: "test",
        },
    },
    babel_config: #{
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 512,
        ramdisks: []
    },
    firewall: #{
        enabled: true,
        default_in: "deny",
        default_out: "allow",
        rules: [],
    },
};
"#;
}
