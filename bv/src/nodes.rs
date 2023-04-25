use anyhow::{anyhow, bail, Context, Result};
use babel_api::metadata::{firewall, Requirements};
use babel_api::rhai_plugin;
use chrono::{DateTime, Utc};
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{self, read_dir};
use tokio::sync::RwLock;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::pal::{NetInterface, Pal};
use crate::services::cookbook::BABEL_PLUGIN_NAME;
use crate::{
    config::SharedConfig,
    node::{build_registry_dir, Node},
    node_data::{NodeData, NodeImage, NodeProperties, NodeStatus},
    node_metrics,
    services::{api::pb, cookbook::CookbookService, keyfiles::KeyService},
    BV_VAR_PATH,
};

pub const REGISTRY_CONFIG_FILENAME: &str = "nodes.json";

fn id_not_found(id: Uuid) -> anyhow::Error {
    anyhow!("Node with id `{}` not found", id)
}

fn name_not_found(name: &str) -> anyhow::Error {
    anyhow!("Node with name `{}` not found", name)
}

pub fn build_registry_filename(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_FILENAME)
}

#[derive(Clone, Debug)]
pub enum ServiceStatus {
    Enabled,
    Disabled,
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
}

#[derive(Debug)]
pub struct Nodes<P: Pal + Debug> {
    api_config: SharedConfig,
    pub nodes: RwLock<HashMap<Uuid, RwLock<Node<P>>>>,
    node_data_cache: RwLock<HashMap<Uuid, NodeDataCache>>,
    node_ids: RwLock<HashMap<String, Uuid>>,
    data: RwLock<CommonData>,
    pal: Arc<P>,
}

#[derive(Debug)]
pub struct NodeConfig {
    pub name: String,
    pub image: NodeImage,
    pub ip: String,
    pub gateway: String,
    pub rules: Vec<firewall::Rule>,
    pub properties: NodeProperties,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct CommonData {
    machine_index: u32,
}

impl<P: Pal + Debug> Nodes<P> {
    #[instrument(skip(self))]
    pub async fn create(&self, id: Uuid, config: NodeConfig) -> Result<()> {
        if self.nodes.read().await.contains_key(&id) {
            warn!("Node with id `{id}` exists");
            return Ok(());
        }

        if self.node_ids.read().await.contains_key(&config.name) {
            bail!("Node with name `{}` exists", config.name);
        }

        let ip = config
            .ip
            .parse()
            .with_context(|| format!("invalid ip {} for node {}", config.ip, id))?;
        let gateway = config
            .gateway
            .parse()
            .with_context(|| format!("invalid gateway {} for node {}", config.gateway, id))?;

        for n in self.nodes.read().await.values() {
            if n.read().await.data.network_interface.ip() == &ip {
                bail!("Node with ip address `{ip}` exists");
            }
        }

        let requirements = self
            .fetch_image_data(&config.image)
            .await
            .with_context(|| "fetch image data failed")?;
        let network_interface = self.create_network_interface(ip, gateway).await?;

        let node_data_cache = NodeDataCache {
            name: config.name.clone(),
            image: config.image.clone(),
            ip: network_interface.ip().to_string(),
            gateway: network_interface.gateway().to_string(),
            started_at: None,
        };

        let node_data = NodeData {
            id,
            name: config.name.clone(),
            image: config.image,
            expected_status: NodeStatus::Stopped,
            started_at: None,
            network_interface,
            requirements,
            self_update: false,
            properties: config.properties,
            firewall_rules: config.rules,
            initialized: false,
        };
        self.save().await?;

        let node = Node::create(self.pal.clone(), node_data).await?;
        self.nodes.write().await.insert(id, RwLock::new(node));
        self.node_ids.write().await.insert(config.name, id);
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
            let new_requirements = self.fetch_image_data(&image).await?;

            let nodes_lock = self.nodes.read().await;
            let mut node = nodes_lock
                .get(&id)
                .ok_or_else(|| id_not_found(id))?
                .write()
                .await;

            let need_to_restart = node.status() == NodeStatus::Running;
            self.node_stop(&mut node).await?;

            let data = &node.data;
            if image.protocol != data.image.protocol {
                bail!("Cannot upgrade protocol to `{}`", image.protocol);
            }
            if image.node_type != data.image.node_type {
                bail!("Cannot upgrade node type to `{}`", image.node_type);
            }
            if data.requirements.vcpu_count != new_requirements.vcpu_count
                || data.requirements.mem_size_mb != new_requirements.mem_size_mb
                || data.requirements.disk_size_gb != new_requirements.disk_size_gb
            {
                bail!("Cannot upgrade node requirements");
            }

            node.upgrade(&image).await?;
            debug!("Node upgraded");

            let mut cache = self.node_data_cache.write().await;
            cache.entry(id).and_modify(|data| {
                data.image = image;
            });

            if need_to_restart {
                self.node_start(&mut node).await?;
            }
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn fetch_image_data(&self, image: &NodeImage) -> Result<Requirements> {
        if !CookbookService::is_image_cache_valid(self.pal.bv_root(), image)
            .await
            .with_context(|| format!("Failed to check image cache: `{image:?}`"))?
        {
            let mut cookbook_service =
                CookbookService::connect(self.pal.bv_root().to_path_buf(), &self.api_config)
                    .await?;

            cookbook_service.download_babel_plugin(image).await?;
            cookbook_service.download_image(image).await?;
            cookbook_service.download_kernel(image).await?;
        }
        info!("Reading blockchain requirements...");
        let folder = CookbookService::get_image_download_folder_path(self.pal.bv_root(), image);
        let path = folder.join(BABEL_PLUGIN_NAME);
        let script = fs::read_to_string(path).await?;
        Ok(rhai_plugin::read_metadata(&script)?.requirements)
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
    pub async fn start(&self, id: Uuid) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut node = nodes_lock
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        self.node_start(&mut node).await
    }

    async fn node_start(&self, node: &mut Node<P>) -> Result<()> {
        if NodeStatus::Running != node.expected_status() {
            node.start().await?;
            debug!("Node started");

            if !node.data.initialized {
                let secret_keys = match self.exchange_keys(node).await {
                    Ok(secret_keys) => secret_keys,
                    Err(e) => {
                        error!("Failed to retrieve keys when starting node: `{e}`");
                        HashMap::new()
                    }
                };

                node.babel_engine.init(secret_keys).await?;
                node.data.initialized = true;
                node.data.save(self.pal.bv_root()).await?;
            }
            // We save the `running` status only after all of the previous steps have succeeded.
            node.set_expected_status(NodeStatus::Running).await?;
            node.set_started_at(Some(Utc::now())).await?;
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn update(
        &self,
        id: Uuid,
        self_update: Option<bool>,
        rules: Vec<firewall::Rule>,
    ) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.update(self_update, rules).await
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
    pub async fn call_method(&self, id: Uuid, method: &str, param: &str) -> Result<String> {
        let nodes = self.nodes.read().await;
        let mut node = nodes
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;
        node.babel_engine.call_method(method, param).await
    }

    #[instrument(skip(self))]
    pub async fn stop(&self, id: Uuid) -> Result<()> {
        let nodes_lock = self.nodes.read().await;
        let mut node = nodes_lock
            .get(&id)
            .ok_or_else(|| id_not_found(id))?
            .write()
            .await;

        self.node_stop(&mut node).await
    }

    async fn node_stop(&self, node: &mut Node<P>) -> Result<()> {
        if NodeStatus::Stopped != node.expected_status() {
            node.stop().await?;
            debug!("Node stopped");
            node.set_expected_status(NodeStatus::Stopped).await?;
            node.set_started_at(None).await?;
        }
        Ok(())
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
            let new = Node::create(self.pal.clone(), node_data).await?;
            self.nodes.write().await.insert(id, RwLock::new(new));
            info!("Recovery: node with ID `{id}` recreated");
        }
        Ok(())
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

    /// Synchronizes the keys in the key server with the keys available locally. Returns a
    /// refreshed set of all keys.
    async fn exchange_keys(&self, node: &mut Node<P>) -> Result<HashMap<String, Vec<u8>>> {
        let mut key_service = KeyService::connect(&self.api_config).await?;

        let api_keys: HashMap<String, Vec<u8>> = key_service
            .download_keys(node.id())
            .await?
            .into_iter()
            .map(|k| (k.name, k.content))
            .collect();
        let api_keys_set: HashSet<&String> = HashSet::from_iter(api_keys.keys());
        debug!("Received API keys: {api_keys_set:?}");

        let node_keys: HashMap<String, Vec<u8>> = node
            .babel_engine
            .download_keys()
            .await?
            .into_iter()
            .map(|k| (k.name, k.content))
            .collect();
        let node_keys_set: HashSet<&String> = HashSet::from_iter(node_keys.keys());
        debug!("Received Node keys: {node_keys_set:?}");

        // Keys present in API, but not on Node, will be sent to Node
        let keys1: Vec<_> = api_keys_set
            .difference(&node_keys_set)
            .map(|n| babel_api::babel::BlockchainKey {
                name: n.to_string(),
                content: api_keys.get(*n).unwrap().to_vec(), // checked
            })
            .collect();
        if !keys1.is_empty() {
            node.babel_engine.upload_keys(keys1).await?;
        }

        // Keys present on Node, but not in API, will be sent to API
        let keys2: Vec<_> = node_keys_set
            .difference(&api_keys_set)
            .map(|n| pb::Keyfile {
                name: n.to_string(),
                content: node_keys.get(*n).unwrap().to_vec(), // checked
            })
            .collect();
        if !keys2.is_empty() {
            key_service.upload_keys(node.id(), keys2).await?;
        }

        // Generate keys if we should (and can)
        if api_keys_set.is_empty()
            && node_keys_set.is_empty()
            && node.babel_engine.has_capability("generate_keys").await?
        {
            node.babel_engine.generate_keys().await?;
            let gen_keys: Vec<_> = node
                .babel_engine
                .download_keys()
                .await?
                .into_iter()
                .map(|k| pb::Keyfile {
                    name: k.name,
                    content: k.content,
                })
                .collect();
            key_service.upload_keys(node.id(), gen_keys.clone()).await?;
            return Ok(gen_keys.into_iter().map(|k| (k.name, k.content)).collect());
        }

        let all_keys = api_keys.into_iter().chain(node_keys.into_iter()).collect();
        Ok(all_keys)
    }

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
                Self::load_nodes(pal.clone(), &registry_dir).await?;

            Self {
                api_config,
                data: RwLock::new(data),
                nodes: RwLock::new(nodes),
                node_ids: RwLock::new(node_ids),
                node_data_cache: RwLock::new(node_data_cache),
                pal,
            }
        } else {
            let nodes = Self {
                api_config,
                data: RwLock::new(CommonData { machine_index: 0 }),
                nodes: Default::default(),
                node_ids: Default::default(),
                node_data_cache: Default::default(),
                pal,
            };
            nodes.save().await?;
            nodes
        })
    }

    async fn load_data(registry_path: &Path) -> Result<CommonData> {
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
                .and_then(|data| async { Node::attach(pal.clone(), data).await })
                .await
            {
                Ok(node) => {
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
        Ok((nodes, node_ids, node_data_cache))
    }

    async fn save(&self) -> Result<()> {
        let registry_path = build_registry_filename(self.pal.bv_root());
        // We only save the common data file. The individual node data files save themselves.
        info!(
            "Writing nodes common config file: {}",
            registry_path.display()
        );
        let config = serde_json::to_string(&*self.data.read().await)?;
        fs::write(&*registry_path, &*config).await?;

        Ok(())
    }

    /// Create and return the next network interface using machine index
    async fn create_network_interface(
        &self,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<<P as Pal>::NetInterface> {
        let mut data = self.data.write().await;
        data.machine_index += 1;
        let iface = self
            .pal
            .create_net_interface(data.machine_index, ip, gateway)
            .await
            .context(format!(
                "failed to create VM bridge bv{}",
                data.machine_index
            ))?;

        Ok(iface)
    }
}
