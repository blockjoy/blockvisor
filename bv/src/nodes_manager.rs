use crate::{
    bv_config::SharedConfig,
    command_failed,
    commands::{self, into_internal, Error},
    cpu_registry::CpuRegistry,
    firewall,
    node::Node,
    node_context::{build_nodes_dir, NODES_DIR},
    node_metrics,
    node_state::{ConfigUpdate, NodeState, VmConfig, VmStatus, NODE_STATE_FILENAME},
    pal::Pal,
    scheduler,
    scheduler::{Action, Scheduled, Scheduler},
    utils, BV_VAR_PATH,
};
use babel_api::{engine::JobInfo, engine::JobsInfo};
use eyre::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Debug,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::{
    fs::{self, read_dir},
    sync::{mpsc, RwLock, RwLockReadGuard},
};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

pub const STATE_FILENAME: &str = "state.json";
const MAX_SUPPORTED_RULES: usize = 128;

pub fn build_state_filename(bv_root: &Path) -> PathBuf {
    bv_root
        .join(BV_VAR_PATH)
        .join(NODES_DIR)
        .join(STATE_FILENAME)
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum MaybeNode<P: Pal> {
    Node(RwLock<Node<P>>),
    BrokenNode(NodeState),
}

#[derive(Debug)]
pub struct NodesManager<P: Pal + Debug> {
    api_config: SharedConfig,
    nodes: Arc<RwLock<HashMap<Uuid, MaybeNode<P>>>>,
    scheduler: Scheduler,
    cpu_registry: CpuRegistry,
    node_state_cache: RwLock<HashMap<Uuid, NodeState>>,
    node_ids: RwLock<HashMap<String, Uuid>>,
    state: RwLock<State>,
    state_path: PathBuf,
    pal: Arc<P>,
}

pub type NodesDataCache = Vec<(Uuid, NodeState)>;

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
    #[serde(default)]
    pub scheduled_tasks: Vec<Scheduled>,
}

impl State {
    async fn load(nodes_path: &Path) -> Result<Self> {
        info!("Reading nodes common config file: {}", nodes_path.display());
        let config = fs::read_to_string(&nodes_path)
            .await
            .context("failed to read nodes state")?;
        serde_json::from_str(&config).context("failed to parse nodes state")
    }

    async fn save(&self, nodes_path: &Path) -> Result<()> {
        info!("Writing nodes common config file: {}", nodes_path.display());
        let config = serde_json::to_string(self).map_err(into_internal)?;
        utils::careful_save(nodes_path, config.as_bytes())
            .await
            .map_err(into_internal)?;

        Ok(())
    }
}

impl<P> NodesManager<P>
where
    P: Pal + Send + Sync + Debug + 'static,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    pub async fn load(pal: P, api_config: SharedConfig) -> Result<Self> {
        let bv_root = pal.bv_root();
        let nodes_dir = build_nodes_dir(bv_root);
        if !nodes_dir.exists() {
            fs::create_dir_all(&nodes_dir)
                .await
                .map_err(into_internal)?;
        }
        let state_path = build_state_filename(bv_root);
        let pal = Arc::new(pal);
        let nodes = Arc::new(RwLock::new(HashMap::new()));
        let available_cpus = pal.available_cpus().await;
        let cpu_registry = CpuRegistry::new(available_cpus);
        Ok(if state_path.exists() {
            let state = State::load(&state_path).await?;
            let scheduler = Scheduler::start(
                &state.scheduled_tasks,
                scheduler::NodeTaskHandler(nodes.clone()),
            );
            let (loaded_nodes, node_ids, node_state_cache) = Self::load_nodes(
                pal.clone(),
                api_config.clone(),
                &nodes_dir,
                scheduler.tx(),
                cpu_registry.clone(),
            )
            .await?;
            *nodes.write().await = loaded_nodes;
            Self {
                api_config,
                state: RwLock::new(state),
                nodes,
                scheduler,
                cpu_registry,
                node_ids: RwLock::new(node_ids),
                node_state_cache: RwLock::new(node_state_cache),
                state_path,
                pal,
            }
        } else {
            let scheduler = Scheduler::start(&[], scheduler::NodeTaskHandler(nodes.clone()));
            let nodes = Self {
                api_config,
                state: RwLock::new(State {
                    scheduled_tasks: vec![],
                }),
                nodes,
                scheduler,
                cpu_registry,
                node_ids: Default::default(),
                node_state_cache: Default::default(),
                state_path,
                pal,
            };
            nodes.state.read().await.save(&nodes.state_path).await?;
            nodes
        })
    }

    pub async fn detach(self) {
        let nodes_lock = self.nodes.read().await;
        for (id, node) in nodes_lock.iter() {
            if let MaybeNode::Node(node) = node {
                if let Err(err) = node.write().await.detach().await {
                    warn!("error while detaching node {id}: {err:#}")
                }
            }
        }
        match self.scheduler.stop().await {
            Ok(tasks) => {
                let mut state = self.state.write().await;
                state.scheduled_tasks = tasks;
                if let Err(err) = state.save(&self.state_path).await {
                    error!("error saving nodes state: {err:#}");
                }
            }
            Err(err) => error!("error stopping scheduler: {err:#}"),
        }
    }

    pub async fn nodes_list(&self) -> RwLockReadGuard<'_, HashMap<Uuid, MaybeNode<P>>> {
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
    pub async fn create(&self, mut desired_state: NodeState) -> commands::Result<NodeState> {
        check_babel_version(&desired_state.image.min_babel_version)?;
        let id = desired_state.id;
        let mut node_ids = self.node_ids.write().await;
        if let Some(cache) = self.node_state_cache.read().await.get(&id) {
            warn!("node with id `{id}` exists");
            return Ok(cache.clone());
        }

        if node_ids.contains_key(&desired_state.name) {
            command_failed!(Error::Internal(anyhow!(
                "node with name `{}` exists",
                desired_state.name
            )));
        }

        check_firewall_rules(&desired_state.firewall.rules)?;
        if !desired_state.dev_mode {
            self.check_node_requirements(&desired_state, None).await?;
        } else {
            desired_state.vm_config.vcpu_count = 0;
        }

        for n in self.nodes.read().await.values() {
            let node_ip = match n {
                MaybeNode::Node(node) => node.read().await.state.ip,
                MaybeNode::BrokenNode(state) => state.ip,
            };
            if node_ip == desired_state.ip {
                command_failed!(Error::Internal(anyhow!(
                    "node with ip address `{}` exists",
                    desired_state.ip
                )));
            }
        }

        let node = Node::create(
            self.pal.clone(),
            self.api_config.clone(),
            desired_state,
            self.scheduler.tx(),
            self.cpu_registry.clone(),
        )
        .await;
        let node = node?;
        let node_state = node.state.clone();
        self.nodes
            .write()
            .await
            .insert(id, MaybeNode::Node(RwLock::new(node)));
        node_ids.insert(node_state.name.clone(), id);
        self.node_state_cache
            .write()
            .await
            .insert(id, node_state.clone());
        debug!("Node with id `{}` created", id);

        Ok(node_state)
    }

    #[instrument(skip(self))]
    pub async fn upgrade(
        &self,
        mut desired_state: NodeState,
    ) -> commands::Result<(NodeState, VmStatus)> {
        check_babel_version(&desired_state.image.min_babel_version)?;
        let id = desired_state.id;
        let nodes_lock = self.nodes.read().await;
        let MaybeNode::Node(node_lock) = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?
        else {
            command_failed!(Error::Internal(anyhow!(
                "cannot upgrade broken node `{id}`"
            )));
        };

        let read_node = node_lock.read().await;
        if desired_state.image.id == read_node.state.image.id {
            Ok((read_node.state.clone(), read_node.status().await))
        } else {
            let node = read_node.state.clone();
            drop(read_node);

            if desired_state.image.store_key != node.image.store_key {
                command_failed!(Error::Internal(anyhow!(
                    "cannot upgrade node to version that uses different data set: `{}`",
                    desired_state.image.store_key
                )));
            }
            if !node.dev_mode {
                self.check_node_requirements(&desired_state, Some(&node.vm_config))
                    .await?;
            } else {
                desired_state.vm_config.vcpu_count = 0;
            }

            let mut node = node_lock.write().await;
            node.upgrade(desired_state).await?;
            self.node_state_cache
                .write()
                .await
                .insert(id, node.state.clone());
            Ok((node.state.clone(), node.status().await))
        }
    }

    #[instrument(skip(self))]
    pub async fn delete(&self, id: Uuid) -> commands::Result<()> {
        let name = {
            let nodes_lock = self.nodes.read().await;
            let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
            let MaybeNode::Node(node_lock) = maybe_node else {
                command_failed!(Error::Internal(anyhow!("cannot delete broken node `{id}`")));
            };
            let mut node = node_lock.write().await;
            node.delete().await?;
            node.state.name.clone()
        };
        self.nodes.write().await.remove(&id);
        self.node_ids.write().await.remove(&name);
        self.node_state_cache.write().await.remove(&id);
        if let Err(err) = self.scheduler.tx().send(Action::DeleteNode(id)).await {
            error!("Failed to delete node associated tasks form scheduler: {err:#}");
        }
        debug!("Node deleted");
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn start(&self, id: Uuid, reload_plugin: bool) -> commands::Result<()> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            command_failed!(Error::Internal(anyhow!("cannot start broken node `{id}`")));
        };
        let mut node = node_lock.write().await;
        if reload_plugin {
            node.reload_plugin()
                .await
                .map_err(|err| BabelError::Internal { err })
                .map_err(into_internal)?;
        }
        if VmStatus::Running != node.expected_status() {
            let was_initialised = node.state.initialized;
            node.start().await?;
            if reload_plugin && was_initialised {
                node.babel_engine.reload_plugin_config().await?;
            }
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn stop(&self, id: Uuid, force: bool) -> commands::Result<()> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            command_failed!(Error::Internal(anyhow!("cannot stop broken node `{id}`")));
        };
        let mut node = node_lock.write().await;
        if VmStatus::Stopped != node.expected_status() || force {
            node.stop(force).await?;
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn restart(&self, id: Uuid, force: bool) -> commands::Result<()> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            command_failed!(Error::Internal(anyhow!(
                "cannot restart broken node `{id}`"
            )));
        };

        let mut node = node_lock.write().await;
        node.restart(force).await?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn update(&self, id: Uuid, config_update: ConfigUpdate) -> commands::Result<()> {
        if let Some(config) = config_update.new_firewall.as_ref() {
            check_firewall_rules(&config.rules)?;
        }
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            command_failed!(Error::Internal(anyhow!("cannot update broken node `{id}`")));
        };
        let mut node = node_lock.write().await;
        node.update(config_update).await?;
        self.node_state_cache
            .write()
            .await
            .insert(id, node.state.clone());
        Ok(())
    }

    #[instrument(skip(self))]
    async fn expected_status(&self, id: Uuid) -> Result<VmStatus> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        Ok(match maybe_node {
            MaybeNode::Node(node_lock) => {
                let node = node_lock.read().await;
                node.expected_status()
            }
            MaybeNode::BrokenNode(state) => state.expected_status,
        })
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
        for (id, node_lock) in nodes_lock.iter().filter_map(|(id, maybe_node)| {
            if let MaybeNode::Node(node) = maybe_node {
                Some((id, node))
            } else {
                None
            }
        }) {
            if let Ok(mut node) = node_lock.try_write() {
                if node.status().await == VmStatus::Failed
                    && node.expected_status() != VmStatus::Failed
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
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot get jobs for broken node `{id}`");
        };
        let mut node = node_lock.write().await;
        node.babel_engine.get_jobs().await
    }

    #[instrument(skip(self))]
    pub async fn job_info(&self, id: Uuid, job_name: &str) -> Result<JobInfo> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot get job info for broken node `{id}`");
        };
        let mut node = node_lock.write().await;
        node.babel_engine.job_info(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn start_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot start job on broken node `{id}`");
        };
        let mut node = node_lock.write().await;
        node.babel_engine.start_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn stop_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot stop job on broken node `{id}`");
        };
        let mut node = node_lock.write().await;
        node.babel_engine.stop_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn skip_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot skip job on broken node `{id}`");
        };
        let mut node = node_lock.write().await;
        node.babel_engine.skip_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn cleanup_job(&self, id: Uuid, job_name: &str) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot cleanup job on broken node `{id}`");
        };
        let mut node = node_lock.write().await;
        node.babel_engine.cleanup_job(job_name).await
    }

    #[instrument(skip(self))]
    pub async fn metrics(&self, id: Uuid) -> Result<node_metrics::Metric> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot get metrics for broken node `{id}`");
        };
        let mut node = node_lock.write().await;

        node_metrics::collect_metric(&mut node.babel_engine)
            .await
            .ok_or(anyhow!("metrics not available"))
    }

    #[instrument(skip(self))]
    pub async fn capabilities(&self, id: Uuid) -> Result<Vec<String>> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            bail!("Cannot get broken node capabilities `{id}`");
        };
        let node = node_lock.read().await;
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
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound)
            .map_err(|err| BabelError::Internal { err: err.into() })?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            return Err(BabelError::Internal {
                err: anyhow!("Cannot call method '{method}' on broken node {id}"),
            });
        };
        let mut node = node_lock.write().await;

        if reload_plugin {
            node.reload_plugin()
                .await
                .map_err(|err| BabelError::Internal { err })?;
            if node.expected_status() == VmStatus::Running {
                // reevaluate plugin config if node is already started
                node.babel_engine
                    .reload_plugin_config()
                    .await
                    .map_err(|err| BabelError::Internal { err })?;
            }
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

    #[instrument(skip(self))]
    pub async fn reload_plugin(&self, id: Uuid) -> eyre::Result<(), BabelError> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock
            .get(&id)
            .ok_or_else(|| Error::NodeNotFound)
            .map_err(|err| BabelError::Internal { err: err.into() })?;
        let MaybeNode::Node(node_lock) = maybe_node else {
            return Err(BabelError::Internal {
                err: anyhow!("Cannot reload plugin for broken node {id}"),
            });
        };
        let mut node = node_lock.write().await;

        node.reload_plugin()
            .await
            .map_err(|err| BabelError::Internal { err })?;
        if node.expected_status() == VmStatus::Running {
            // reevaluate plugin config if node is already started
            node.babel_engine
                .reload_plugin_config()
                .await
                .map_err(|err| BabelError::Internal { err })?;
        }
        Ok(())
    }

    /// Check if we have enough resources on the host to create/upgrade the node
    ///
    /// Optional tolerance parameter is useful if we want to allow some overbooking.
    /// It also can be used if we want to upgrade the node that exists.
    #[instrument(skip(self))]
    async fn check_node_requirements(
        &self,
        state: &NodeState,
        tolerance: Option<&VmConfig>,
    ) -> commands::Result<()> {
        if self.api_config.config.read().await.maintenance_mode {
            // skip requirements check in maintenance mode
            return Ok(());
        }
        let mut available = self
            .pal
            .available_resources(self.nodes_data_cache().await)
            .await?;
        debug!("Available resources {available:?}");

        if let Some(tol) = tolerance {
            available.disk_size_gb += tol.disk_size_gb;
            available.mem_size_mb += tol.mem_size_mb;
            available.vcpu_count += tol.vcpu_count;
        }

        if state.vm_config.disk_size_gb > available.disk_size_gb {
            command_failed!(Error::Internal(anyhow!(
                "not enough disk space to allocate for the node: required={}, available={}",
                state.vm_config.disk_size_gb,
                available.disk_size_gb
            )));
        }
        if state.vm_config.mem_size_mb > available.mem_size_mb {
            command_failed!(Error::Internal(anyhow!(
                "not enough memory to allocate for the node"
            )));
        }
        if state.vm_config.vcpu_count > available.vcpu_count {
            command_failed!(Error::Internal(anyhow!(
                "not enough vcpu to allocate for the node"
            )));
        }
        Ok(())
    }

    pub async fn node_state_cache(&self, id: Uuid) -> commands::Result<NodeState> {
        let cache = self
            .node_state_cache
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::NodeNotFound)?;

        Ok(cache)
    }

    pub async fn update_node_state_cache(&self, node_state: NodeState) {
        self.node_state_cache
            .write()
            .await
            .insert(node_state.id, node_state);
    }

    pub async fn nodes_data_cache(&self) -> NodesDataCache {
        self.node_state_cache
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
        nodes_dir: &Path,
        tx: mpsc::Sender<scheduler::Action>,
        cpu_registry: CpuRegistry,
    ) -> Result<(
        HashMap<Uuid, MaybeNode<P>>,
        HashMap<String, Uuid>,
        HashMap<Uuid, NodeState>,
    )> {
        info!("Reading nodes config dir: {}", nodes_dir.display());
        let mut nodes = HashMap::new();
        let mut node_ids = HashMap::new();
        let mut node_state_cache = HashMap::new();
        let node_state_path = |path: &Path| {
            if path.is_dir() {
                let state_path = path.join(NODE_STATE_FILENAME);
                if state_path.exists() {
                    Some(state_path)
                } else {
                    None
                }
            } else {
                None
            }
        };
        let mut dir = read_dir(nodes_dir)
            .await
            .context("failed to read nodes state dir")?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .context("failed to read nodes state entry")?
        {
            let Some(path) = node_state_path(&entry.path()) else {
                continue;
            };
            match NodeState::load(&path).await {
                Ok(state) => {
                    // insert node and its info into internal data structures
                    let id = state.id;
                    let name = state.name.clone();
                    node_ids.insert(name.clone(), id);
                    node_state_cache.insert(id, state.clone());
                    nodes.insert(
                        id,
                        match Node::attach(
                            pal.clone(),
                            api_config.clone(),
                            state.clone(),
                            tx.clone(),
                            cpu_registry.clone(),
                        )
                        .await
                        {
                            Ok(node) => MaybeNode::Node(RwLock::new(node)),
                            Err(err) => {
                                error!("Failed to attach node {id}: {err:#}");
                                MaybeNode::BrokenNode(state)
                            }
                        },
                    );
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

        Ok((nodes, node_ids, node_state_cache))
    }

    async fn status(&self, id: Uuid) -> Result<VmStatus> {
        let nodes_lock = self.nodes.read().await;
        let maybe_node = nodes_lock.get(&id).ok_or_else(|| Error::NodeNotFound)?;
        Ok(if let MaybeNode::Node(node_lock) = maybe_node {
            let node = node_lock.read().await;
            node.status().await
        } else {
            VmStatus::Failed
        })
    }
}

fn check_babel_version(min_babel_version: &str) -> commands::Result<()> {
    let parse_err = |err| {
        Error::Internal(anyhow!(
            "failed to parse min_babel_version={min_babel_version}: {err:#}"
        ))
    };
    let current_version = env!("CARGO_PKG_VERSION");
    let mut min_babel_version = semver::Version::parse(min_babel_version).map_err(parse_err)?;
    min_babel_version.patch = 0; // It is fine to use BV with older version if it is only patch difference.
    let version = semver::Version::parse(current_version).map_err(parse_err)?;
    if version < min_babel_version {
        command_failed!(Error::Internal(anyhow!("image require never version of babel '{min_babel_version}' while current is '{current_version}' - upgrade bv first")));
    }
    Ok(())
}

fn check_firewall_rules(rules: &[firewall::Rule]) -> commands::Result<()> {
    if rules.len() > MAX_SUPPORTED_RULES {
        command_failed!(Error::Internal(anyhow!(
            "can't configure more than {MAX_SUPPORTED_RULES} rules!"
        )));
    }
    crate::firewall::check_rules(rules)?;
    Ok(())
}

fn name_not_found(name: &str) -> eyre::Error {
    anyhow!("Node with name `{}` not found", name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_state::{CpuAssignmentUpdate, StateBackup, UpgradeState, UpgradeStep};
    use crate::scheduler::Task;
    use crate::{node::tests::*, node_context, pal, pal::VmState, services::api::pb};
    use assert_fs::TempDir;
    use eyre::bail;
    use mockall::*;
    use std::net::IpAddr;
    use std::str::FromStr;

    mock! {
        pub TestImageService {}

        #[tonic::async_trait]
        impl pb::image_service_server::ImageService for TestImageService {
            async fn add_image(
                &self,
                request: tonic::Request<pb::ImageServiceAddImageRequest>,
            ) -> Result<tonic::Response<pb::ImageServiceAddImageResponse>, tonic::Status>;
            async fn get_image(
                &self,
                request: tonic::Request<pb::ImageServiceGetImageRequest>,
            ) -> Result<tonic::Response<pb::ImageServiceGetImageResponse>, tonic::Status>;
            async fn list_archives(
                &self,
                request: tonic::Request<pb::ImageServiceListArchivesRequest>,
            ) -> Result<
                tonic::Response<pb::ImageServiceListArchivesResponse>,
                tonic::Status,
            >;
            async fn update_archive(
                &self,
                request: tonic::Request<pb::ImageServiceUpdateArchiveRequest>,
            ) -> Result<
                tonic::Response<pb::ImageServiceUpdateArchiveResponse>,
                tonic::Status,
            >;
            async fn update_image(
                &self,
                request: tonic::Request<pb::ImageServiceUpdateImageRequest>,
            ) -> Result<
                tonic::Response<pb::ImageServiceUpdateImageResponse>,
                tonic::Status,
            >;
        }
    }

    struct TestEnv {
        tmp_root: PathBuf,
        default_plugin_path: PathBuf,
        _async_panic_checker: bv_tests_utils::AsyncPanicChecker,
    }

    impl TestEnv {
        async fn new() -> Result<Self> {
            let tmp_root = TempDir::new()?.to_path_buf();
            fs::create_dir_all(&tmp_root).await?;

            Ok(Self {
                tmp_root,
                default_plugin_path: testing_babel_path_absolute(),
                _async_panic_checker: Default::default(),
            })
        }

        fn default_pal(&self) -> MockTestPal {
            default_pal(self.tmp_root.clone())
        }
    }

    const TEST_NODE_REQUIREMENTS: pal::AvailableResources = pal::AvailableResources {
        vcpu_count: 1,
        mem_size_mb: 2048,
        disk_size_gb: 1,
    };

    fn available_test_resources(
        _nodes_data_cache: NodesDataCache,
    ) -> Result<pal::AvailableResources> {
        Ok(TEST_NODE_REQUIREMENTS)
    }

    fn add_create_node_expectations(
        pal: &mut MockTestPal,
        expected_index: u32,
        state: NodeState,
        vm_mock: MockTestVM,
    ) {
        let id = state.id;
        pal.expect_available_resources()
            .withf(move |req| expected_index - 1 == req.len() as u32)
            .once()
            .returning(available_test_resources);
        add_firewall_expectation(pal, state.clone());
        pal.expect_create_vm()
            .with(predicate::eq(default_bv_context()), predicate::eq(state))
            .return_once(move |_, _| Ok(vm_mock));
        pal.expect_create_node_connection()
            .with(predicate::eq(id))
            .return_once(dummy_connection_mock);
    }

    fn add_create_node_fail_vm_expectations(
        pal: &mut MockTestPal,
        expected_index: u32,
        state: NodeState,
    ) {
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
        pal.expect_create_vm()
            .with(predicate::eq(default_bv_context()), predicate::eq(state))
            .return_once(|_, _| bail!("failed to create vm"));
        pal.expect_cleanup_firewall_config().returning(|_| Ok(()));
    }

    pub fn build_node_state(name: &str, ip: &str, gateway: &str) -> NodeState {
        let mut state = default_node_state();
        state.id = Uuid::new_v4();
        state.expected_status = VmStatus::Stopped;
        state.name = name.to_string();
        state.ip = IpAddr::from_str(ip).unwrap();
        state.gateway = IpAddr::from_str(gateway).unwrap();
        state
    }

    #[tokio::test]
    async fn test_create_node_and_delete() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        pal.expect_available_cpus().return_const(3usize);
        let config = default_config(test_env.tmp_root.clone());

        let mut first_node_state =
            build_node_state("first node name", "192.168.0.7", "192.168.0.1");
        first_node_state.assigned_cpus = vec![2];
        let mut vm_mock = MockTestVM::new();
        let plugin_path = test_env.default_plugin_path.clone();
        vm_mock
            .expect_plugin_path()
            .returning(move || plugin_path.clone());
        vm_mock.expect_node_env().returning(Default::default);
        vm_mock.expect_state().once().return_const(VmState::SHUTOFF);
        vm_mock
            .expect_delete()
            .once()
            .returning(|| bail!("delete VM failed"));
        vm_mock.expect_delete().once().returning(|| Ok(()));
        add_create_node_expectations(&mut pal, 1, first_node_state.clone(), vm_mock);

        let mut second_node_state =
            build_node_state("second node name", "192.168.0.8", "192.168.0.1");
        second_node_state.assigned_cpus = vec![1];
        let mut vm_mock = MockTestVM::new();
        let plugin_path = test_env.default_plugin_path.clone();
        vm_mock
            .expect_plugin_path()
            .returning(move || plugin_path.clone());
        vm_mock.expect_node_env().returning(Default::default);
        vm_mock.expect_state().once().return_const(VmState::SHUTOFF);
        add_create_node_expectations(&mut pal, 2, second_node_state.clone(), vm_mock);

        let mut failed_node_state =
            build_node_state("failed node name", "192.168.0.9", "192.168.0.1");
        failed_node_state.assigned_cpus = vec![0];
        add_create_node_fail_vm_expectations(&mut pal, 3, failed_node_state.clone());

        let nodes = NodesManager::load(pal, config).await?;
        assert!(nodes.nodes_list().await.is_empty());

        let mut not_supported_state = first_node_state.clone();
        not_supported_state.image.min_babel_version = "777.77.7".to_string();
        assert_eq!(
            format!(
                "BV internal error: 'image require never version of babel '777.77.0' while current is '{}' - upgrade bv first'",
                env!("CARGO_PKG_VERSION")
            ),
            nodes
                .create(not_supported_state)
                .await
                .unwrap_err()
                .to_string()
        );

        nodes.create(first_node_state.clone()).await?;
        nodes.create(second_node_state.clone()).await?;
        nodes.create(second_node_state.clone()).await?;
        assert_eq!(
            "BV internal error: 'failed to check available resources'",
            nodes
                .create(failed_node_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: 'not enough memory to allocate for the node'",
            nodes
                .create(failed_node_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: 'failed to create vm'",
            nodes
                .create(failed_node_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(1, nodes.cpu_registry.len().await);
        let mut duplicated_node_state = first_node_state.clone();
        duplicated_node_state.id = Uuid::new_v4();
        assert_eq!(
            "BV internal error: 'node with name `first node name` exists'",
            nodes
                .create(duplicated_node_state)
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: 'node with ip address `192.168.0.7` exists'",
            nodes
                .create(build_node_state("node name", "192.168.0.7", "192.168.0.1"))
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
                ips: vec![],
                ports: vec![],
            })
            .collect::<Vec<_>>();
        let mut too_many_rules_state = build_node_state("node name", "192.168.0.9", "192.168.0.1");
        too_many_rules_state.firewall.rules = rules;
        assert_eq!(
            "BV internal error: 'can't configure more than 128 rules!'",
            nodes
                .create(too_many_rules_state)
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(2, nodes.nodes_list().await.len());
        assert_eq!(
            second_node_state.id,
            nodes.node_id_for_name(&second_node_state.name).await?
        );
        assert_eq!(VmStatus::Stopped, nodes.status(first_node_state.id).await?);
        assert_eq!(VmStatus::Stopped, nodes.status(second_node_state.id).await?);
        assert_eq!(
            VmStatus::Stopped,
            nodes.expected_status(first_node_state.id).await?
        );
        assert_eq!(
            first_node_state,
            nodes.node_state_cache(first_node_state.id).await?
        );

        assert_eq!(
            "BV internal error: 'delete VM failed'",
            nodes
                .delete(first_node_state.id)
                .await
                .unwrap_err()
                .to_string()
        );
        nodes
            .scheduler
            .tx()
            .send(Action::Add(Scheduled {
                node_id: first_node_state.id,
                name: "task".to_string(),
                schedule: cron::Schedule::from_str("1 * * * * * *").unwrap(),
                task: Task::PluginFnCall {
                    name: "scheduled_fn".to_string(),
                    param: "scheduled_param".to_string(),
                },
            }))
            .await
            .unwrap();
        nodes.delete(first_node_state.id).await.unwrap();
        assert!(!nodes
            .node_state_cache
            .read()
            .await
            .contains_key(&first_node_state.id));
        assert!(nodes.scheduler.stop().await.unwrap().is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_load() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        pal.expect_available_cpus().return_const(1usize);
        let config = default_config(test_env.tmp_root.clone());

        let node_state = default_node_state();
        fs::create_dir_all(node_context::build_node_dir(pal.bv_root(), node_state.id)).await?;

        let nodes = NodesManager::load(pal, config).await?;
        assert!(nodes.nodes_list().await.is_empty());

        let mut invalid_node_state = node_state.clone();
        invalid_node_state.id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047531").unwrap();
        invalid_node_state.name = "invalid node".to_string();
        let nodes_dir = build_nodes_dir(&test_env.tmp_root);
        make_node_dir(&nodes_dir, node_state.id).await;
        node_state.save(&nodes_dir).await?;
        make_node_dir(&nodes_dir, invalid_node_state.id).await;
        invalid_node_state.save(&nodes_dir).await?;

        fs::create_dir_all(nodes_dir.join("4931bafa-92d9-4521-9fc6-a77eee047533"))
            .await
            .unwrap();
        fs::write(
            nodes_dir.join("4931bafa-92d9-4521-9fc6-a77eee047533/state.json"),
            "invalid node data",
        )
        .await?;

        let mut pal = test_env.default_pal();
        pal.expect_available_cpus().return_const(1usize);
        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .returning(dummy_connection_mock);
        let plugin_path = test_env.default_plugin_path.clone();
        pal.expect_attach_vm()
            .with(
                predicate::eq(default_bv_context()),
                predicate::eq(node_state.clone()),
            )
            .returning(move |_, _| {
                let mut vm = MockTestVM::new();
                let plugin_path = plugin_path.clone();
                vm.expect_plugin_path()
                    .returning(move || plugin_path.clone());
                vm.expect_node_env().returning(Default::default);
                vm.expect_state().return_const(VmState::SHUTOFF);
                Ok(vm)
            });
        pal.expect_create_node_connection()
            .with(predicate::eq(invalid_node_state.id))
            .returning(dummy_connection_mock);
        pal.expect_attach_vm()
            .with(
                predicate::eq(default_bv_context()),
                predicate::eq(invalid_node_state.clone()),
            )
            .returning(|_, _| {
                bail!("failed to attach");
            });
        let config = default_config(test_env.tmp_root.clone());
        let nodes = NodesManager::load(pal, config).await?;
        assert_eq!(2, nodes.nodes_list().await.len());
        assert_eq!(
            "node name",
            nodes.node_state_cache(node_state.id).await?.name
        );
        assert_eq!(
            VmStatus::Running,
            nodes.expected_status(node_state.id).await?
        );
        assert_eq!(node_state.id, nodes.node_id_for_name("node name").await?);
        assert_eq!(VmStatus::Failed, nodes.status(invalid_node_state.id).await?);
        assert_eq!(
            "BV internal error: 'cannot stop broken node `4931bafa-92d9-4521-9fc6-a77eee047531`'",
            nodes
                .stop(invalid_node_state.id, true)
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            VmStatus::Failed,
            nodes.status(invalid_node_state.id).await.unwrap()
        );
        assert_eq!(
            VmStatus::Running,
            nodes.expected_status(invalid_node_state.id).await.unwrap()
        );
        assert_eq!(
            invalid_node_state.name,
            nodes
                .node_state_cache(invalid_node_state.id)
                .await
                .unwrap()
                .name
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_upgrade_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());

        let mut node_state = build_node_state("node name", "192.168.0.7", "192.168.0.1");
        node_state.assigned_cpus = vec![4];
        let mut new_state = node_state.clone();
        new_state.initialized = false;
        new_state.image.id = "new-image-id".to_string();
        new_state.image.uri = "new.uri".to_string();
        new_state.image.version = "3.2.1".to_string();
        new_state.vm_config.vcpu_count = 2;
        new_state.vm_config.mem_size_mb = 4096;
        new_state.vm_config.disk_size_gb = 3;
        new_state.assigned_cpus = vec![4, 3];
        let mut vm_mock = MockTestVM::new();
        let plugin_path = test_env.default_plugin_path.clone();
        vm_mock
            .expect_plugin_path()
            .once()
            .returning(move || plugin_path.clone());
        vm_mock.expect_node_env().once().returning(Default::default);
        let updated_plugin_path = test_env.tmp_root.join("updated.rhai");
        let mut truncated = fs::read_to_string(test_env.default_plugin_path)
            .await
            .unwrap();
        truncated.truncate(truncated.len() - 30); // truncate info function
        fs::write(&updated_plugin_path, truncated).await.unwrap();
        vm_mock
            .expect_data_dir()
            .once()
            .returning(move || test_env.tmp_root.clone());
        vm_mock
            .expect_plugin_path()
            .once()
            .returning(move || updated_plugin_path.clone());
        vm_mock.expect_node_env().once().returning(Default::default);
        vm_mock
            .expect_state()
            .times(2)
            .return_const(VmState::SHUTOFF);
        vm_mock.expect_upgrade().return_once(|_| Ok(()));
        vm_mock.expect_drop_backup().return_once(|| Ok(()));
        add_create_node_expectations(&mut pal, 1, node_state.clone(), vm_mock);
        pal.expect_available_resources()
            .withf(move |req| 1 == req.len() as u32)
            .once()
            .returning(|_requirements| bail!("failed to get available resources"));
        pal.expect_available_cpus().return_const(5usize);
        pal.expect_available_resources()
            .withf(move |req| 1 == req.len() as u32)
            .times(2)
            .returning(|_| {
                Ok(pal::AvailableResources {
                    vcpu_count: 5,
                    mem_size_mb: 4096,
                    disk_size_gb: 4,
                })
            });
        pal.expect_attach_vm()
            .with(
                predicate::eq(default_bv_context()),
                predicate::eq(new_state.clone()),
            )
            .return_once(move |_, _| Ok(MockTestVM::new()));
        add_firewall_expectation(&mut pal, new_state.clone());

        let nodes = NodesManager::load(pal, config).await?;

        nodes.create(node_state.clone()).await?;
        let mut not_supported_state = new_state.clone();
        not_supported_state.image.min_babel_version = "777.77.7".to_string();
        assert_eq!(
            format!(
                "BV internal error: 'image require never version of babel '777.77.0' while current is '{}' - upgrade bv first'",
                env!("CARGO_PKG_VERSION")
            ),
            nodes
                .upgrade(not_supported_state)
                .await
                .unwrap_err()
                .to_string()
        );

        let not_found_id = uuid::Uuid::new_v4();
        let mut not_found_state = new_state.clone();
        not_found_state.id = not_found_id;
        assert_eq!(
            "node not found",
            nodes
                .upgrade(not_found_state)
                .await
                .unwrap_err()
                .to_string()
        );
        let mut broken_node = node_state.clone();
        broken_node.id = Uuid::new_v4();
        nodes
            .nodes
            .write()
            .await
            .insert(broken_node.id, MaybeNode::BrokenNode(broken_node.clone()));
        assert_eq!(node_state, nodes.node_state_cache(node_state.id).await?);
        {
            let mut nodes_list = nodes.nodes.write().await;
            let MaybeNode::Node(node) = nodes_list.get_mut(&node_state.id).unwrap() else {
                panic!("unexpected broken node")
            };
            let mut node = node.write().await;
            node.state.initialized = true;
            assert!(node.babel_engine.has_capability("info"));
        }
        assert_eq!(
            format!(
                "BV internal error: 'cannot upgrade broken node `{}`'",
                broken_node.id
            ),
            nodes.upgrade(broken_node).await.unwrap_err().to_string()
        );
        let mut new_archive_node_state = new_state.clone();
        new_archive_node_state.image.store_key = "different_store_key".to_string();
        assert_eq!(
            "BV internal error: 'cannot upgrade node to version that uses different data set: `different_store_key`'",
            nodes
                .upgrade(new_archive_node_state)
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "BV internal error: 'failed to get available resources'",
            nodes
                .upgrade(new_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        let mut cpu_devourer_node_state = new_state.clone();
        cpu_devourer_node_state.image.id = "another-id".to_string();
        cpu_devourer_node_state.vm_config.vcpu_count = 2048;
        assert_eq!(
            "BV internal error: 'not enough vcpu to allocate for the node'",
            nodes
                .upgrade(cpu_devourer_node_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );

        nodes.upgrade(new_state.clone()).await?;
        let updated_state = nodes.node_state_cache(new_state.id).await?;
        let mut state_backup: StateBackup = node_state.clone().into();
        state_backup.initialized = true;
        new_state.upgrade_state = UpgradeState {
            active: false,
            state_backup: Some(state_backup),
            need_rollback: None,
            steps: vec![
                UpgradeStep::CpuAssignment(CpuAssignmentUpdate::AcquiredCpus(1)),
                UpgradeStep::Vm,
                UpgradeStep::Plugin,
                UpgradeStep::Firewall,
            ],
            data_stamp: None,
        };
        assert_eq!(new_state, updated_state);
        {
            let mut nodes_list = nodes.nodes.write().await;
            let MaybeNode::Node(node) = nodes_list.get_mut(&node_state.id).unwrap() else {
                panic!("unexpected broken node")
            };
            let node = node.write().await;
            assert!(!node.state.initialized);
            assert!(!node.babel_engine.has_capability("info"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_recovery() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let mut node_state = default_node_state();
        node_state.expected_status = VmStatus::Stopped;
        let node_id = node_state.id;

        pal.expect_available_cpus().return_const(1usize);
        pal.expect_available_resources()
            .withf(move |req| req.is_empty())
            .once()
            .returning(available_test_resources);
        add_firewall_expectation(&mut pal, node_state.clone());
        let plugin_path = test_env.default_plugin_path.clone();
        pal.expect_create_vm().return_once(move |_, _| {
            let mut mock = MockTestVM::new();
            let plugin_path = plugin_path.clone();
            let mut seq = Sequence::new();
            mock.expect_plugin_path()
                .times(1)
                .in_sequence(&mut seq)
                .returning(move || plugin_path.clone());
            mock.expect_node_env()
                .times(1)
                .in_sequence(&mut seq)
                .returning(Default::default);
            mock.expect_state()
                .times(6)
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            mock.expect_start()
                .once()
                .in_sequence(&mut seq)
                .returning(|| bail!("failed to start VM"));
            mock.expect_state()
                .once()
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

        sut.nodes.create(node_state.clone()).await?;
        // no recovery needed - node is expected to be stopped
        sut.nodes.recover().await;

        // no recovery for permanently failed node
        sut.on_node(|node| node.state.expected_status = VmStatus::Failed)
            .await;
        sut.nodes.recover().await;

        // recovery of node that is expected to be running, but it is not
        sut.on_node(|node| node.state.expected_status = VmStatus::Running)
            .await;
        sut.nodes.recover().await;

        // recovery of node that is expected to be stopped, but it is not
        sut.on_node(|node| node.state.expected_status = VmStatus::Stopped)
            .await;
        sut.nodes.recover().await;

        // node connection recovery
        sut.on_node(|node| {
            node.state.expected_status = VmStatus::Running;
            node.state.initialized = true;
            node.post_recovery();
        })
        .await;
        sut.nodes.recover().await;

        // no recovery needed - node is expected to be running
        sut.on_node(|node| {
            node.state.expected_status = VmStatus::Running;
            node.state.initialized = true;
        })
        .await;
        sut.nodes.recover().await;

        Ok(())
    }

    struct RecoverySut {
        node_id: Uuid,
        nodes: NodesManager<MockTestPal>,
    }

    impl RecoverySut {
        async fn on_node(&mut self, call_on_node: impl FnOnce(&mut Node<MockTestPal>)) {
            let mut nodes_lock = self.nodes.nodes.write().await;
            let MaybeNode::Node(node) = nodes_lock.get_mut(&self.node_id).unwrap() else {
                panic!("unexpected broken node")
            };
            call_on_node(&mut *node.write().await);
        }
    }
}
