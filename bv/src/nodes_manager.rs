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
pub struct NodesManager<P: Pal + Debug> {
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
#[derive(Clone, Debug, PartialEq)]
pub struct NodeDataCache {
    pub name: String,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub started_at: Option<DateTime<Utc>>,
    pub standalone: bool,
}

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

impl<P: Pal + Debug> NodesManager<P> {
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
            let new_meta = self.fetch_image_data(&image).await?;
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
    pub async fn recover(&self) {
        let nodes_lock = self.nodes.read().await;
        for (id, node_lock) in nodes_lock.iter() {
            if let Ok(mut node) = node_lock.try_write() {
                if node.status() == NodeStatus::Failed
                    && node.expected_status() != NodeStatus::Failed
                {
                    if let Err(e) = node.recover().await {
                        error!("node `{id}` recovery failed with: {e}");
                    }
                }
            }
        }
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
    /// Optional tolerance parameter is useful if we want to allow some overbooking.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        pal::{
            BabelClient, BabelSupClient, CommandsStream, NodeConnection, ServiceConnector,
            VirtualMachine, VmState,
        },
        services::{
            self, api::pb, api::pb::ArchiveLocation, cookbook::ROOT_FS_FILE, kernel::KERNELS_DIR,
            AuthToken,
        },
        utils::tests::test_channel,
    };
    use assert_fs::TempDir;
    use async_trait::async_trait;
    use bv_utils::cmd::run_cmd;
    use mockall::*;
    use std::ffi::OsStr;
    use std::str::FromStr;
    use std::{path::Path, time::Duration};
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::Channel;

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
        pub TestCookbookService {}

        #[tonic::async_trait]
        impl pb::cookbook_service_server::CookbookService for TestCookbookService {
            async fn retrieve_plugin(
                &self,
                request: tonic::Request<pb::CookbookServiceRetrievePluginRequest>,
            ) -> Result<
                tonic::Response<pb::CookbookServiceRetrievePluginResponse>,
                tonic::Status,
            >;
            async fn retrieve_image(
                &self,
                request: tonic::Request<pb::CookbookServiceRetrieveImageRequest>,
            ) -> Result<
                tonic::Response<pb::CookbookServiceRetrieveImageResponse>,
                tonic::Status,
            >;
            async fn requirements(
                &self,
                request: tonic::Request<pb::CookbookServiceRequirementsRequest>,
            ) -> Result<
                tonic::Response<pb::CookbookServiceRequirementsResponse>,
                tonic::Status,
            >;
            async fn net_configurations(
                &self,
                request: tonic::Request<pb::CookbookServiceNetConfigurationsRequest>,
            ) -> Result<
                tonic::Response<pb::CookbookServiceNetConfigurationsResponse>,
                tonic::Status,
            >;
            async fn list_babel_versions(
                &self,
                request: tonic::Request<pb::CookbookServiceListBabelVersionsRequest>,
            ) -> Result<
                tonic::Response<pb::CookbookServiceListBabelVersionsResponse>,
                tonic::Status,
            >;
        }
    }

    #[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
    pub struct DummyNet {
        pub name: String,
        pub ip: IpAddr,
        pub gateway: IpAddr,
        pub remaster_error: Option<String>,
        pub delete_error: Option<String>,
    }

    #[async_trait]
    impl NetInterface for DummyNet {
        fn name(&self) -> &String {
            &self.name
        }
        fn ip(&self) -> &IpAddr {
            &self.ip
        }
        fn gateway(&self) -> &IpAddr {
            &self.gateway
        }
        async fn remaster(&self) -> Result<()> {
            if let Some(err) = &self.remaster_error {
                bail!(err.clone())
            } else {
                Ok(())
            }
        }
        async fn delete(self) -> Result<()> {
            if let Some(err) = self.delete_error {
                bail!(err)
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    pub struct EmptyStreamConnector;
    pub struct EmptyStream;

    #[async_trait]
    impl ServiceConnector<EmptyStream> for EmptyStreamConnector {
        async fn connect(&self) -> Result<EmptyStream> {
            Ok(EmptyStream)
        }
    }

    #[async_trait]
    impl CommandsStream for EmptyStream {
        async fn wait_for_pending_commands(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    #[derive(Clone)]
    pub struct TestConnector {
        tmp_root: PathBuf,
    }

    #[async_trait]
    impl services::ApiServiceConnector for TestConnector {
        async fn connect<T, I>(&self, with_interceptor: I) -> Result<T>
        where
            I: Send + Sync + Fn(Channel, AuthToken) -> T,
        {
            Ok(with_interceptor(
                test_channel(&self.tmp_root),
                AuthToken("test_token".to_owned()),
            ))
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestNodeConnection {}

        #[async_trait]
        impl NodeConnection for TestNodeConnection {
            async fn open(&mut self, _max_delay: Duration) -> Result<()>;
            fn close(&mut self);
            fn is_closed(&self) -> bool;
            fn mark_broken(&mut self);
            fn is_broken(&self) -> bool;
            async fn test(&self) -> Result<()>;
            async fn babelsup_client<'a>(&'a mut self) -> Result<&'a mut BabelSupClient>;
            async fn babel_client<'a>(&'a mut self) -> Result<&'a mut BabelClient>;
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestVM {}

        #[async_trait]
        impl VirtualMachine for TestVM {
            fn state(&self) -> VmState;
            async fn delete(self) -> Result<()>;
            async fn shutdown(&mut self) -> Result<()>;
            async fn force_shutdown(&mut self) -> Result<()>;
            async fn start(&mut self) -> Result<()>;
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestPal {}

        #[tonic::async_trait]
        impl Pal for TestPal {
            fn bv_root(&self) -> &Path;
            fn babel_path(&self) -> &Path;
            fn job_runner_path(&self) -> &Path;
            type NetInterface = DummyNet;
            async fn create_net_interface(
                &self,
                index: u32,
                ip: IpAddr,
                gateway: IpAddr,
                config: &SharedConfig,
            ) -> Result<DummyNet>;

            type CommandsStream = EmptyStream;
            type CommandsStreamConnector = EmptyStreamConnector;
            fn create_commands_stream_connector(
                &self,
                config: &SharedConfig,
            ) -> EmptyStreamConnector;

            type ApiServiceConnector = TestConnector;
            fn create_api_service_connector(&self, config: &SharedConfig) -> TestConnector;

            type NodeConnection = MockTestNodeConnection;
            fn create_node_connection(&self, node_id: Uuid) -> MockTestNodeConnection;

            type VirtualMachine = MockTestVM;
            async fn create_vm(
                &self,
                node_data: &NodeData<DummyNet>,
            ) -> Result<MockTestVM>;
            async fn attach_vm(
                &self,
                node_data: &NodeData<DummyNet>,
            ) -> Result<MockTestVM>;
            fn build_vm_data_path(&self, id: Uuid) -> PathBuf;
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
            pal
        }

        fn default_config(&self) -> SharedConfig {
            SharedConfig::new(
                Config {
                    id: "host_id".to_string(),
                    token: "token".to_string(),
                    refresh_token: "refresh_token".to_string(),
                    blockjoy_api_url: "api.url".to_string(),
                    blockjoy_mqtt_url: Some("mqtt.url".to_string()),
                    update_check_interval_secs: None,
                    blockvisor_port: 888,
                    iface: "bvbr7".to_string(),
                    cluster_id: None,
                    cluster_seed_urls: None,
                },
                self.tmp_root.clone(),
            )
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
            utils::tests::TestServer,
            mockito::ServerGuard,
            Vec<mockito::Mock>,
        ) {
            self.generate_dummy_archive().await;
            let mut http_server = mockito::Server::new();
            let mut http_mocks = vec![];

            // expect image retrieve and download for all images, but only once
            let mut cookbook = MockTestCookbookService::new();
            for (test_image, rhai_content) in images {
                let node_image = Some(pb::ConfigIdentifier {
                    protocol: test_image.protocol.clone(),
                    node_type: pb::NodeType::from_str(&test_image.node_type)
                        .unwrap()
                        .into(),
                    node_version: test_image.node_version.clone(),
                });
                let expected_image = node_image.clone();
                let resp = tonic::Response::new(pb::CookbookServiceRetrievePluginResponse {
                    plugin: Some(pb::Plugin {
                        identifier: expected_image.clone(),
                        rhai_content,
                    }),
                });
                cookbook
                    .expect_retrieve_plugin()
                    .withf(move |req| req.get_ref().id == expected_image)
                    .return_once(move |_| Ok(resp));
                let url = http_server.url();
                let expected_image = node_image.clone();
                cookbook
                    .expect_retrieve_image()
                    .withf(move |req| req.get_ref().id == expected_image)
                    .return_once(move |_| {
                        Ok(tonic::Response::new(
                            pb::CookbookServiceRetrieveImageResponse {
                                location: Some(ArchiveLocation {
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
                        location: Some(ArchiveLocation {
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

            let socket_path = self.tmp_root.join("test_socket");
            let (tx, rx) = tokio::sync::oneshot::channel();
            (
                utils::tests::TestServer {
                    tx,
                    handle: tokio::spawn(async move {
                        let uds_stream = UnixListenerStream::new(
                            tokio::net::UnixListener::bind(socket_path).unwrap(),
                        );
                        tonic::transport::server::Server::builder()
                            .max_concurrent_streams(1)
                            .add_service(pb::kernel_service_server::KernelServiceServer::new(
                                kernels,
                            ))
                            .add_service(pb::cookbook_service_server::CookbookServiceServer::new(
                                cookbook,
                            ))
                            .serve_with_incoming_shutdown(uds_stream, async {
                                rx.await.ok();
                            })
                            .await
                            .unwrap();
                    }),
                },
                http_server,
                http_mocks,
            )
        }
    }

    fn add_create_node_expectations(
        pal: &mut MockTestPal,
        expected_index: u32,
        id: Uuid,
        config: NodeConfig,
    ) {
        let expected_ip = config.ip.clone();
        let expected_gateway = config.gateway.clone();
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
                    delete_error: None,
                })
            });
        pal.expect_create_vm()
            .with(predicate::eq(expected_node_data(
                expected_index,
                id,
                config,
                None,
            )))
            .return_once(|_| {
                let mut mock = MockTestVM::new();
                mock.expect_state().once().return_const(VmState::SHUTOFF);
                Ok(mock)
            });
        pal.expect_create_node_connection()
            .with(predicate::eq(id))
            .return_once(|_| MockTestNodeConnection::new());
    }

    fn add_create_node_fail_vm_expectations(
        pal: &mut MockTestPal,
        expected_index: u32,
        id: Uuid,
        config: NodeConfig,
    ) {
        let expected_ip = config.ip.clone();
        let expected_gateway = config.gateway.clone();
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
                    delete_error: None,
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
            image: image.unwrap_or(config.image),
            kernel: TEST_KERNEL.to_string(),
            network_interface: DummyNet {
                name: format!("bv{expected_index}"),
                ip: IpAddr::from_str(&config.ip).unwrap(),
                gateway: IpAddr::from_str(&config.gateway).unwrap(),
                remaster_error: None,
                delete_error: None,
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 2048,
                disk_size_gb: 1,
            },
            firewall_rules: config.rules,
            properties: config.properties,
            network: config.network,
            standalone: config.standalone,
        }
    }

    #[tokio::test]
    async fn test_create_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = test_env.default_config();

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
        };
        add_create_node_expectations(&mut pal, 1, first_node_id, first_node_config.clone());

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
        };
        add_create_node_expectations(&mut pal, 2, second_node_id, second_node_config.clone());

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
            .start_test_server(vec![
                (
                    test_env.test_image.clone(),
                    include_bytes!("../../babel_api/protocols/testing/babel.rhai").to_vec(),
                ),
                (
                    NodeImage {
                        protocol: "huge_blockchain".to_string(),
                        node_type: "validator".to_string(),
                        node_version: "1.2.3".to_string(),
                    },
                    HUGE_IMAGE_RHAI.to_owned().into_bytes(),
                ),
            ])
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
            "Not enough disk space to allocate for the node",
            nodes
                .create(
                    failed_node_id,
                    NodeConfig {
                        name: "huge node name".to_string(),
                        image: NodeImage {
                            protocol: "huge_blockchain".to_string(),
                            node_type: "validator".to_string(),
                            node_version: "1.2.3".to_string(),
                        },
                        ip: "192.168.0.9".to_string(),
                        gateway: "192.168.0.1".to_string(),
                        rules: vec![],
                        properties: Default::default(),
                        network: "test".to_string(),
                        standalone: false,
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "failed to create vm",
            nodes
                .create(failed_node_id, failed_node_config.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "failed to create VM bridge bv4",
            nodes
                .create(failed_node_id, failed_node_config)
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "Node with name `first node name` exists",
            nodes
                .create(failed_node_id, first_node_config.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "Node with ip address `192.168.0.7` exists",
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
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "invalid ip `invalid`",
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
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "invalid gateway `invalid`",
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
            "Can't configure more than 128 rules!",
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
                    }
                )
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "fetch image data failed",
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
                        standalone: true,
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
            },
            nodes.node_data_cache(first_node_id).await?
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
        let config = test_env.default_config();

        let nodes = NodesManager::load(pal, config).await?;
        assert!(nodes.nodes_list().await.is_empty());

        let node_data = NodeData {
            id: Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap(),
            name: "first node".to_string(),
            expected_status: NodeStatus::Stopped,
            started_at: None,
            initialized: false,
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
        };
        let mut invalid_node_data = node_data.clone();
        invalid_node_data.id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047531").unwrap();
        let registry_dir = build_registry_dir(&test_env.tmp_root);
        fs::create_dir_all(&registry_dir).await?;
        node_data.save(&registry_dir).await?;
        invalid_node_data.save(&registry_dir).await?;
        fs::copy(
            format!(
                "{}/../babel_api/protocols/testing/babel.rhai",
                env!("CARGO_MANIFEST_DIR")
            ),
            registry_dir.join(format!("{}.rhai", node_data.id)),
        )
        .await?;
        fs::copy(
            format!(
                "{}/../babel_api/protocols/testing/babel.rhai",
                env!("CARGO_MANIFEST_DIR")
            ),
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
            .returning(|_| MockTestNodeConnection::new());
        pal.expect_attach_vm()
            .with(predicate::eq(node_data.clone()))
            .returning(|_| {
                let mut vm = MockTestVM::new();
                vm.expect_state().return_const(VmState::SHUTOFF);
                Ok(vm)
            });
        pal.expect_create_node_connection()
            .with(predicate::eq(invalid_node_data.id))
            .returning(|_| MockTestNodeConnection::new());
        pal.expect_attach_vm()
            .with(predicate::eq(invalid_node_data.clone()))
            .returning(|_| {
                bail!("failed to attach");
            });
        let config = test_env.default_config();
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
        let config = test_env.default_config();

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
        add_create_node_expectations(&mut pal, 1, node_id, node_config.clone());
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
                    include_bytes!("../../babel_api/protocols/testing/babel.rhai").to_vec(),
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
            },
            nodes.node_data_cache(node_id).await?
        );
        {
            let mut nodes_list = nodes.nodes.write().await;
            let mut node = nodes_list.get_mut(&node_id).unwrap().write().await;
            node.data.initialized = true;
            assert!(node.babel_engine.has_capability("info").await?);
        }
        nodes.upgrade(node_id, new_image.clone()).await?;
        assert_eq!(
            NodeDataCache {
                name: node_config.name,
                image: new_image.clone(),
                ip: node_config.ip,
                gateway: node_config.gateway,
                started_at: None,
                standalone: node_config.standalone,
            },
            nodes.node_data_cache(node_id).await?
        );
        {
            let mut nodes_list = nodes.nodes.write().await;
            let mut node = nodes_list.get_mut(&node_id).unwrap().write().await;
            assert!(!node.data.initialized);
            assert!(!node.babel_engine.has_capability("info").await?);
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
            KernelService::get_kernel_path(&test_env.tmp_root, TEST_KERNEL),
            KernelService::get_kernel_path(&test_env.tmp_root, "5.10.175"),
        )
        .await?;
        assert_eq!(
            "Cannot upgrade kernel",
            nodes
                .upgrade(node_id, invalid_kernel_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "Cannot upgrade disk requirements",
            nodes
                .upgrade(node_id, invalid_disk_size_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "Not enough vcpu to allocate for the node",
            nodes
                .upgrade(node_id, cpu_devourer_image.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "Cannot upgrade protocol to `differen_chain`",
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
            "Cannot upgrade node type to `node`",
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
        let config = test_env.default_config();
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
        };

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
            mock.expect_state().times(7).return_const(VmState::SHUTOFF);
            mock.expect_state().times(3).return_const(VmState::RUNNING);
            Ok(mock)
        });
        pal.expect_create_node_connection().return_once(|_| {
            let mut mock = MockTestNodeConnection::new();
            mock.expect_babel_client()
                .times(1)
                .returning(|| bail!("no babel client"));
            mock.expect_is_broken().times(1).returning(|| true);
            mock.expect_test().times(1).returning(|| Ok(()));
            mock.expect_is_closed().times(2).returning(|| false);
            mock
        });

        let mut sut = RecoverySut {
            node_id,
            nodes: NodesManager::load(pal, config).await?,
        };

        let (test_server, _http_server, http_mocks) = test_env
            .start_test_server(vec![(
                test_env.test_image.clone(),
                include_bytes!("../../babel_api/protocols/testing/babel.rhai").to_vec(),
            )])
            .await;
        sut.nodes.create(node_id, node_config.clone()).await?;
        sut.nodes.recover().await;

        sut.on_node(|node| node.data.expected_status = NodeStatus::Failed)
            .await;
        sut.nodes.recover().await;

        sut.on_node(|node| node.data.expected_status = NodeStatus::Running)
            .await;
        sut.nodes.recover().await;

        sut.on_node(|node| node.data.expected_status = NodeStatus::Stopped)
            .await;
        sut.nodes.recover().await;

        sut.on_node(|node| node.data.expected_status = NodeStatus::Running)
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

    const TEST_KERNEL: &str = "5.10.174-build.1+fc.ufw";
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
        data_directory_mount_point: "/blockjoy/miner/data",
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
    const HUGE_IMAGE_RHAI: &str = r#"
const METADATA = #{
    min_babel_version: "0.0.9",
    kernel: "5.10.174-build.1+fc.ufw",
    node_version: "1.15.9",
    protocol: "huge_blockchain",
    node_type: "validator",
    requirements: #{
        vcpu_count: 1073741824,
        mem_size_mb: 1073741824,
        disk_size_gb: 1073741824,
    },
    nets: #{
        test: #{
            url: "https://testnet-api.helium.wtf/v1/",
            net_type: "test",
        },
    },
    babel_config: #{
        data_directory_mount_point: "/blockjoy/miner/data",
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
        data_directory_mount_point: "/blockjoy/miner/data",
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
