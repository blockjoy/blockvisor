use babel_api::{
    engine::JobInfo,
    metadata::{firewall, BlockchainMetadata, Requirements},
    rhai_plugin,
};
use chrono::{DateTime, Utc};
use eyre::{anyhow, bail, Context, Result};
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
use tokio::sync::RwLockReadGuard;
use tokio::{
    fs::{self, read_dir},
    sync::RwLock,
};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::{
    config::SharedConfig,
    firecracker_machine::FC_BIN_NAME,
    hosts::HostInfo,
    node::{build_registry_dir, Node},
    node_data::{NodeData, NodeImage, NodeProperties, NodeStatus},
    node_metrics,
    pal::{NetInterface, Pal},
    services::{
        cookbook::{CookbookService, BABEL_PLUGIN_NAME},
        kernel::KernelService,
    },
    utils, BV_VAR_PATH,
};

pub const REGISTRY_CONFIG_FILENAME: &str = "nodes.json";
const MAX_SUPPORTED_RULES: usize = 128;

pub fn build_registry_filename(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_FILENAME)
}

#[derive(Debug)]
pub struct Nodes<P: Pal + Debug> {
    api_config: SharedConfig,
    nodes: RwLock<HashMap<Uuid, RwLock<Node<P>>>>,
    node_data_cache: RwLock<HashMap<Uuid, NodeDataCache>>,
    node_ids: RwLock<HashMap<String, Uuid>>,
    state: RwLock<State>,
    pal: Arc<P>,
}

/// Container with some shallow information about the node
///
/// This information is [mostly] immutable, and we can cache it for
/// easier access in case some node is locked and we cannot access
/// it's actual data right away
#[derive(Clone, Debug)]
pub struct NodeDataCache {
    pub name: String,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub started_at: Option<DateTime<Utc>>,
    pub standalone: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NodeConfig {
    pub name: String,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub rules: Vec<firewall::Rule>,
    pub properties: NodeProperties,
    pub network: String,
    pub standalone: bool,
}

#[derive(Error, Debug)]
pub enum BabelError {
    #[error("given method not found")]
    MethodNotFound,
    #[error("BV plugin error: {err}")]
    Plugin { err: eyre::Error },
    #[error("BV internal error: {err}")]
    Internal { err: eyre::Error },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct State {
    machine_index: u32,
}

impl<P: Pal + Debug> Nodes<P> {
    pub async fn load(pal: P, api_config: SharedConfig) -> Result<Self> {
        let bv_root = pal.bv_root();
        let registry_dir = build_registry_dir(bv_root);
        if !registry_dir.exists() {
            fs::create_dir_all(&registry_dir).await?;
        }
        let registry_path = build_registry_filename(bv_root);
        let pal = Arc::new(pal);
        Ok(if registry_path.exists() {
            let data = Self::load_data(&registry_path).await?;
            let (nodes, node_ids, node_data_cache) =
                Self::load_nodes(pal.clone(), api_config.clone(), &registry_dir).await?;

            Self {
                api_config,
                state: RwLock::new(data),
                nodes: RwLock::new(nodes),
                node_ids: RwLock::new(node_ids),
                node_data_cache: RwLock::new(node_data_cache),
                pal,
            }
        } else {
            let nodes = Self {
                api_config,
                state: RwLock::new(State { machine_index: 0 }),
                nodes: Default::default(),
                node_ids: Default::default(),
                node_data_cache: Default::default(),
                pal,
            };
            nodes.save_state().await?;
            nodes
        })
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
    pub async fn create(&self, id: Uuid, config: NodeConfig) -> Result<()> {
        let mut node_ids = self.node_ids.write().await;
        if self.nodes.read().await.contains_key(&id) {
            warn!("Node with id `{id}` exists");
            return Ok(());
        }

        if node_ids.contains_key(&config.name) {
            bail!("Node with name `{}` exists", config.name);
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
            .map(|(k, v)| (k.to_uppercase(), v))
            .collect();

        for n in self.nodes.read().await.values() {
            let node = n.read().await;
            if node.data.network_interface.ip() == &ip {
                bail!("Node with ip address `{ip}` exists");
            }
        }

        let meta = self
            .fetch_image_data(&config.image)
            .await
            .with_context(|| "fetch image data failed")?;

        self.check_node_requirements(&meta.requirements, None)
            .await?;

        let network_interface = self.create_network_interface(ip, gateway).await?;

        let node_data_cache = NodeDataCache {
            name: config.name.clone(),
            image: config.image.clone(),
            ip: network_interface.ip().to_string(),
            gateway: network_interface.gateway().to_string(),
            started_at: None,
            standalone: config.standalone,
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
        };
        self.save_state().await?;

        let node = Node::create(self.pal.clone(), self.api_config.clone(), node_data).await?;
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
    pub async fn upgrade(&self, id: Uuid, image: NodeImage) -> Result<()> {
        if image != self.image(id).await? {
            let new_meta = self.fetch_image_data(&image).await?;

            let nodes_lock = self.nodes.read().await;
            let data = nodes_lock
                .get(&id)
                .ok_or_else(|| id_not_found(id))?
                .read()
                .await
                .data
                .clone();

            if image.protocol != data.image.protocol {
                bail!("Cannot upgrade protocol to `{}`", image.protocol);
            }
            if image.node_type != data.image.node_type {
                bail!("Cannot upgrade node type to `{}`", image.node_type);
            }
            if data.kernel != new_meta.kernel {
                bail!("Cannot upgrade kernel");
            }
            if data.requirements.disk_size_gb != new_meta.requirements.disk_size_gb {
                bail!("Cannot upgrade disk requirements");
            }

            self.check_node_requirements(&new_meta.requirements, Some(&data.requirements))
                .await?;

            let mut node = nodes_lock
                .get(&id)
                .ok_or_else(|| id_not_found(id))?
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
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        if let Some(node_lock) = self.nodes.write().await.remove(&id) {
            let node = node_lock.into_inner();
            self.node_ids.write().await.remove(&node.data.name);
            self.node_data_cache.write().await.remove(&id);
            node.delete().await?;
            debug!("Node deleted");
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn start(&self, id: Uuid, reload_plugin: bool) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut node = nodes_lock
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        if reload_plugin {
            node.reload_plugin()
                .await
                .map_err(|err| BabelError::Internal { err })?;
        }
        if NodeStatus::Running != node.expected_status() {
            node.start().await?;
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn stop(&self, id: Uuid, force: bool) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut node = nodes_lock
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        if NodeStatus::Stopped != node.expected_status() || force {
            node.stop(force).await?;
            debug!("Node stopped");
            node.set_expected_status(NodeStatus::Stopped).await?;
            node.set_started_at(None).await?;
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn update(&self, id: Uuid, rules: Vec<firewall::Rule>) -> Result<()> {
        check_user_firewall_rules(&rules)?;
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.update(rules).await
    }

    #[instrument(skip(self))]
    pub async fn status(&self, id: Uuid) -> Result<NodeStatus> {
        let nodes = self.nodes.read().await;
        let node = nodes.get(&id).ok_or_else(|| id_not_found(id))?.read().await;
        Ok(node.status())
    }

    #[instrument(skip(self))]
    async fn expected_status(&self, id: Uuid) -> Result<NodeStatus> {
        let nodes = self.nodes.read().await;
        let node = nodes.get(&id).ok_or_else(|| id_not_found(id))?.read().await;
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
    pub async fn recover(&self) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut nodes_to_recreate = vec![];
        for (id, node_lock) in nodes_lock.iter() {
            if let Ok(mut node) = node_lock.try_write() {
                if node.status() == NodeStatus::Failed
                    && node.expected_status() != NodeStatus::Failed
                {
                    if !node.is_data_valid().await? {
                        nodes_to_recreate.push(node.data.clone());
                    } else if let Err(e) = node.recover().await {
                        error!("Recovery: node with ID `{id}` failed: {e}");
                    }
                }
            }
        }
        drop(nodes_lock);
        for node_data in nodes_to_recreate {
            let id = node_data.id;
            // If some files are corrupted, the files will be recreated.
            // Some intermediate data could be lost in that case.
            self.fetch_image_data(&node_data.image).await?;
            let new = Node::create(self.pal.clone(), self.api_config.clone(), node_data).await?;
            self.nodes.write().await.insert(id, RwLock::new(new));
            info!("Recovery: node with ID `{id}` recreated");
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn jobs(&self, id: Uuid) -> Result<Vec<(String, JobInfo)>> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.get_jobs().await
    }

    #[instrument(skip(self))]
    pub async fn job_info(&self, id: Uuid, job_name: &str) -> Result<JobInfo> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.job_info(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn start_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.start_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn stop_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.stop_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn cleanup_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.cleanup_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn logs(&self, id: Uuid) -> Result<Vec<String>> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.get_logs().await
    }

    #[instrument(skip(self))]
    pub async fn babel_logs(&self, id: Uuid, max_lines: u32) -> Result<Vec<String>> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.get_babel_logs(max_lines).await
    }

    #[instrument(skip(self))]
    pub async fn metrics(&self, id: Uuid) -> Result<node_metrics::Metric> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;

        let metrics = node_metrics::collect_metric(&mut node.babel_engine).await;
        Ok(metrics)
    }

    #[instrument(skip(self))]
    pub async fn keys(&self, id: Uuid) -> Result<Vec<babel_api::babel::BlockchainKey>> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.download_keys().await
    }

    #[instrument(skip(self))]
    pub async fn capabilities(&self, id: Uuid) -> Result<Vec<String>> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.capabilities().await
    }

    #[instrument(skip(self))]
    pub async fn call_method(
        &self,
        id: Uuid,
        method: &str,
        param: &str,
        reload_plugin: bool,
    ) -> Result<String, BabelError> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))
            .map_err(|err| BabelError::Internal { err })?
            .write()
            .await;

        if reload_plugin {
            node.reload_plugin()
                .await
                .map_err(|err| BabelError::Internal { err })?;
        }
        if !node
            .babel_engine
            .has_capability(method)
            .await
            .map_err(|err| BabelError::Internal { err })?
        {
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
    /// Optinal tolerance parameter is useful if we want to allow some overbooking.
    /// It also can be used if we want to upgrade the node that exists.
    #[instrument(skip(self))]
    async fn check_node_requirements(
        &self,
        requirements: &Requirements,
        tolerance: Option<&Requirements>,
    ) -> Result<()> {
        let host_info = HostInfo::collect()?;

        let mut allocated_disk_size_gb = 0;
        let mut allocated_mem_size_mb = 0;
        let mut allocated_vcpu_count = 0;
        for n in self.nodes.read().await.values() {
            let node = n.read().await;
            allocated_disk_size_gb += node.data.requirements.disk_size_gb;
            allocated_mem_size_mb += node.data.requirements.mem_size_mb;
            allocated_vcpu_count += node.data.requirements.vcpu_count;
        }

        let mut total_disk_size_gb = host_info.disk_space_bytes as usize / 1_000_000_000;
        let mut total_mem_size_mb = host_info.memory_bytes as usize / 1_000_000;
        let mut total_vcpu_count = host_info.cpu_count;
        if let Some(tol) = tolerance {
            total_disk_size_gb += tol.disk_size_gb;
            total_mem_size_mb += tol.mem_size_mb;
            total_vcpu_count += tol.vcpu_count;
        }

        if (allocated_disk_size_gb + requirements.disk_size_gb) > total_disk_size_gb {
            bail!("Not enough disk space to allocate for the node");
        }
        if (allocated_mem_size_mb + requirements.mem_size_mb) > total_mem_size_mb {
            bail!("Not enough memory to allocate for the node");
        }
        if (allocated_vcpu_count + requirements.vcpu_count) > total_vcpu_count {
            bail!("Not enough vcpu to allocate for the node");
        }

        Ok(())
    }

    #[instrument(skip(self))]
    async fn fetch_image_data(&self, image: &NodeImage) -> Result<BlockchainMetadata> {
        let bv_root = self.pal.bv_root();
        let folder = CookbookService::get_image_download_folder_path(bv_root, image);
        let rhai_path = folder.join(BABEL_PLUGIN_NAME);

        let script = if !CookbookService::is_image_cache_valid(bv_root, image)
            .await
            .with_context(|| format!("Failed to check image cache: `{image:?}`"))?
        {
            let mut cookbook_service = CookbookService::connect(
                self.pal.create_api_service_connector(&self.api_config),
                bv_root.to_path_buf(),
            )
            .await
            .with_context(|| "cannot connect to cookbook service")?;
            cookbook_service
                .download_babel_plugin(image)
                .await
                .with_context(|| "cannot download babel plugin")?;
            cookbook_service
                .download_image(image)
                .await
                .with_context(|| "cannot download image")?;
            fs::read_to_string(rhai_path).await?
        } else {
            fs::read_to_string(rhai_path).await?
        };
        let meta = rhai_plugin::read_metadata(&script)?;
        if !KernelService::is_kernel_cache_valid(bv_root, &meta.kernel)
            .await
            .with_context(|| format!("Failed to check kernel cache: `{}`", meta.kernel))?
        {
            let mut kernel_service = KernelService::connect(
                self.pal.create_api_service_connector(&self.api_config),
                bv_root.to_path_buf(),
            )
            .await
            .with_context(|| "cannot connect to kernel service")?;
            kernel_service
                .download_kernel(&meta.kernel)
                .await
                .with_context(|| "cannot download kernel")?;
        }

        info!("Reading blockchain requirements: {:?}", &meta.requirements);
        Ok(meta)
    }

    #[instrument(skip(self))]
    async fn image(&self, id: Uuid) -> Result<NodeImage> {
        let nodes = self.nodes.read().await;
        let node = nodes.get(&id).ok_or_else(|| id_not_found(id))?.read().await;
        Ok(node.data.image.clone())
    }

    pub async fn node_data_cache(&self, id: Uuid) -> Result<NodeDataCache> {
        let cache = self
            .node_data_cache
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| id_not_found(id))?;

        Ok(cache)
    }

    async fn load_data(registry_path: &Path) -> Result<State> {
        info!(
            "Reading nodes common config file: {}",
            registry_path.display()
        );
        let config = fs::read_to_string(&registry_path)
            .await
            .context("failed to read nodes registry")?;
        serde_json::from_str(&config).context("failed to parse nodes registry")
    }

    async fn load_nodes(
        pal: Arc<P>,
        api_config: SharedConfig,
        registry_dir: &Path,
    ) -> Result<(
        HashMap<Uuid, RwLock<Node<P>>>,
        HashMap<String, Uuid>,
        HashMap<Uuid, NodeDataCache>,
    )> {
        info!("Reading nodes config dir: {}", registry_dir.display());
        let mut nodes = HashMap::new();
        let mut node_ids = HashMap::new();
        let mut node_data_cache = HashMap::new();
        let mut fc_processes_to_check = utils::get_all_processes_pids(FC_BIN_NAME)?;
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
                    Node::attach(pal.clone(), api_config.clone(), data).await
                })
                .await
            {
                Ok(node) => {
                    // remove FC pid from list of all discovered FC pids
                    // in the end of load this list should be empty
                    if node.status() == NodeStatus::Running {
                        let node_pid = utils::get_process_pid(FC_BIN_NAME, &node.id().to_string())?;
                        fc_processes_to_check.retain(|p| p != &node_pid);
                    }
                    // insert node and its info into internal data structures
                    let id = node.id();
                    let name = &node.data.name;
                    node_ids.insert(name.clone(), id);
                    node_data_cache.insert(
                        id,
                        NodeDataCache {
                            name: name.clone(),
                            ip: node.data.network_interface.ip().to_string(),
                            gateway: node.data.network_interface.gateway().to_string(),
                            image: node.data.image.clone(),
                            started_at: node.data.started_at,
                            standalone: node.data.standalone,
                        },
                    );
                    nodes.insert(id, RwLock::new(node));
                }
                Err(e) => {
                    // blockvisord should not bail on problems with individual node files.
                    // It should log error though.
                    error!("Failed to load node from file `{}`: {}", path.display(), e);
                }
            };
        }
        // check if we run some unmanaged FC processes on the host
        for pid in fc_processes_to_check {
            error!("Process with id {pid} is not managed by BV");
        }

        Ok((nodes, node_ids, node_data_cache))
    }

    async fn save_state(&self) -> Result<()> {
        let registry_path = build_registry_filename(self.pal.bv_root());
        // We only save the common data file. The individual node data files save themselves.
        info!(
            "Writing nodes common config file: {}",
            registry_path.display()
        );
        let config = serde_json::to_string(&*self.state.read().await)?;
        fs::write(&*registry_path, &*config).await?;

        Ok(())
    }

    /// Create and return the next network interface using machine index
    async fn create_network_interface(
        &self,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<P::NetInterface> {
        let mut data = self.state.write().await;
        data.machine_index += 1;
        let iface = self
            .pal
            .create_net_interface(data.machine_index, ip, gateway, &self.api_config)
            .await
            .context(format!(
                "failed to create VM bridge bv{}",
                data.machine_index
            ))?;

        Ok(iface)
    }
}

fn check_user_firewall_rules(rules: &[firewall::Rule]) -> Result<()> {
    if rules.len() > MAX_SUPPORTED_RULES {
        bail!("Can't configure more than {MAX_SUPPORTED_RULES} rules!");
    }
    babel_api::metadata::check_firewall_rules(rules)?;
    Ok(())
}

fn id_not_found(id: Uuid) -> eyre::Error {
    anyhow!("Node with id `{}` not found", id)
}

fn name_not_found(name: &str) -> eyre::Error {
    anyhow!("Node with name `{}` not found", name)
}
