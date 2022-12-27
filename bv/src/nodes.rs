use anyhow::{anyhow, bail, Context, Result};
use babel_api::config::Babel;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::Path;
use tokio::fs::{self, read_dir};
use tokio::sync::broadcast::{self, Sender};
use tokio::sync::OnceCell;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    env::{REGISTRY_CONFIG_DIR, REGISTRY_CONFIG_FILE},
    network_interface::NetworkInterface,
    node::Node,
    node_data::{NodeData, NodeImage, NodeRequirements, NodeStatus},
    services::{
        cookbook_service::CookbookService,
        grpc::{pb, pb::node_info::ContainerStatus},
        key_service::KeyService,
    },
};

fn id_not_found(id: &Uuid) -> anyhow::Error {
    anyhow!("Node with id `{}` not found", id)
}

fn name_not_found(name: &str) -> anyhow::Error {
    anyhow!("Node with name `{}` not found", name)
}

#[derive(Clone, Debug)]
pub enum ServiceStatus {
    Enabled,
    Disabled,
}

#[derive(Debug)]
pub struct Nodes {
    pub api_config: Config,
    pub nodes: HashMap<Uuid, Node>,
    pub node_ids: HashMap<String, Uuid>,
    data: CommonData,
    tx: OnceCell<Sender<pb::InfoUpdate>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CommonData {
    pub machine_index: u32,
}

impl Nodes {
    #[instrument(skip(self))]
    pub async fn create(
        &mut self,
        id: Uuid,
        name: String,
        image: NodeImage,
        ip: String,
        gateway: String,
    ) -> Result<()> {
        if self.nodes.contains_key(&id) {
            bail!(format!("Node with id `{}` exists", &id));
        }

        if self.node_ids.contains_key(&name) {
            bail!(format!("Node with name `{}` exists", &name));
        }

        let babel = self.fetch_image_data(&image).await?;
        check_babel_version(&babel.config.min_babel_version)?;

        let _ = self.send_container_status(&id, ContainerStatus::Creating);

        self.data.machine_index += 1;
        let ip = ip.parse()?;
        let gateway = gateway.parse()?;
        let network_interface = self.create_network_interface(ip, gateway).await?;

        let requirements = NodeRequirements {
            vcpu_count: babel.requirements.vcpu_count,
            mem_size_mb: babel.requirements.mem_size_mb,
            disk_size_gb: babel.requirements.disk_size_gb,
        };

        let node = NodeData {
            id,
            name: name.clone(),
            image,
            expected_status: NodeStatus::Stopped,
            network_interface,
            requirements,
            self_update: false,
        };
        self.save().await?;

        let node = Node::create(node).await?;
        self.nodes.insert(id, node);
        self.node_ids.insert(name, id);
        debug!("Node with id `{}` created", id);

        let _ = self.send_container_status(&id, ContainerStatus::Stopped);

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn upgrade(&mut self, id: Uuid, image: NodeImage) -> Result<()> {
        let babel = self.fetch_image_data(&image).await?;
        check_babel_version(&babel.config.min_babel_version)?;

        let _ = self.send_container_status(&id, ContainerStatus::Upgrading);

        let need_to_restart = self.status(id).await? == NodeStatus::Running;
        self.stop(id).await?;
        let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(&id))?;

        if image.protocol != node.data.image.protocol {
            bail!("Cannot upgrade protocol to `{}`", image.protocol);
        }
        if image.node_type != node.data.image.node_type {
            bail!("Cannot upgrade node type to `{}`", image.node_type);
        }
        if node.data.requirements.vcpu_count != babel.requirements.vcpu_count
            || node.data.requirements.mem_size_mb != babel.requirements.mem_size_mb
            || node.data.requirements.disk_size_gb != babel.requirements.disk_size_gb
        {
            bail!("Cannot upgrade node requirements");
        }

        node.upgrade(&image).await?;
        debug!("Node upgraded");

        if need_to_restart {
            self.start(id).await?;
        }

        let _ = self.send_container_status(&id, ContainerStatus::Upgraded);

        Ok(())
    }

    #[instrument(skip(self))]
    async fn fetch_image_data(&mut self, image: &NodeImage) -> Result<Babel> {
        if !CookbookService::is_image_cache_valid(image)
            .await
            .with_context(|| format!("Failed to check image cache: `{image:?}`"))?
        {
            let mut cookbook_service = CookbookService::connect(
                &self.api_config.blockjoy_registry_url,
                &self.api_config.token,
            )
            .await?;

            cookbook_service
                .download_babel_config(image)
                .await
                .with_context(|| format!("Failed to download babel config: `{image:?}`"))?;
            cookbook_service
                .download_image(image)
                .await
                .with_context(|| format!("Failed to download kernel: `{image:?}`"))?;
            cookbook_service
                .download_kernel(image)
                .await
                .with_context(|| format!("Failed to download OS image: `{image:?}`"))?;
        }

        let babel = CookbookService::get_babel_config(image).await?;
        Ok(babel)
    }

    #[instrument(skip(self))]
    pub async fn delete(&mut self, id: Uuid) -> Result<()> {
        let node = self.nodes.remove(&id).ok_or_else(|| id_not_found(&id))?;
        self.node_ids.remove(&node.data.name);

        let _ = self.send_container_status(&id, ContainerStatus::Deleting);

        node.delete().await?;
        debug!("Node deleted");

        let _ = self.send_container_status(&id, ContainerStatus::Deleted);

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn start(&mut self, id: Uuid) -> Result<()> {
        let _ = self.send_container_status(&id, ContainerStatus::Starting);

        let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(&id))?;
        node.start().await?;
        debug!("Node started");

        let _ = self.send_container_status(&id, ContainerStatus::Running);

        if let Err(e) = self.exchange_keys(&id).await {
            warn!("Key exchange error: {e:?}")
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn stop(&mut self, id: Uuid) -> Result<()> {
        let _ = self.send_container_status(&id, ContainerStatus::Stopping);

        let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(&id))?;
        node.stop().await?;
        debug!("Node stopped");

        let _ = self.send_container_status(&id, ContainerStatus::Stopped);

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn list(&self) -> Vec<NodeData> {
        debug!("Listing {} nodes", self.nodes.len());

        self.nodes.values().map(|n| n.data.clone()).collect()
    }

    #[instrument(skip(self))]
    pub async fn status(&self, id: Uuid) -> Result<NodeStatus> {
        let node = self.nodes.get(&id).ok_or_else(|| id_not_found(&id))?;

        Ok(node.status())
    }

    pub async fn node_id_for_name(&self, name: &str) -> Result<Uuid> {
        let uuid = self
            .node_ids
            .get(name)
            .cloned()
            .ok_or_else(|| name_not_found(name))?;

        Ok(uuid)
    }

    pub async fn exchange_keys(&mut self, id: &Uuid) -> Result<()> {
        let node = self.nodes.get_mut(id).ok_or_else(|| id_not_found(id))?;

        let mut key_service =
            KeyService::connect(&self.api_config.blockjoy_keys_url, &self.api_config.token).await?;

        let api_keys: HashMap<String, Vec<u8>> = key_service
            .download_keys(id)
            .await?
            .iter()
            .map(|k| (k.name.clone(), k.content.clone()))
            .collect();
        let api_keys_set: HashSet<&String> = HashSet::from_iter(api_keys.keys());
        debug!("Received API keys: {api_keys_set:?}");

        let node_keys: HashMap<String, Vec<u8>> = node
            .download_keys()
            .await?
            .iter()
            .map(|k| (k.name.clone(), k.content.clone()))
            .collect();
        let node_keys_set: HashSet<&String> = HashSet::from_iter(node_keys.keys());
        debug!("Received Node keys: {node_keys_set:?}");

        // Keys present in API, but not on Node, will be sent to Node
        let keys1: Vec<_> = api_keys_set
            .difference(&node_keys_set)
            .into_iter()
            .map(|n| babel_api::BlockchainKey {
                name: n.to_string(),
                content: api_keys.get(*n).unwrap().to_vec(), // checked
            })
            .collect();
        if !keys1.is_empty() {
            node.upload_keys(keys1).await?;
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
            key_service.upload_keys(id, keys2).await?;
        }

        // Generate keys if we should (and can)
        if api_keys_set.is_empty()
            && node_keys_set.is_empty()
            && node
                .has_capability(&babel_api::BabelMethod::GenerateKeys.to_string())
                .await?
        {
            node.generate_keys().await?;
            let gen_keys: Vec<_> = node
                .download_keys()
                .await?
                .iter()
                .map(|k| pb::Keyfile {
                    name: k.name.clone(),
                    content: k.content.clone(),
                })
                .collect();
            key_service.upload_keys(id, gen_keys).await?;
        }

        Ok(())
    }
}

impl Nodes {
    pub fn new(api_config: Config, nodes_data: CommonData) -> Self {
        Self {
            api_config,
            data: nodes_data,
            nodes: HashMap::new(),
            node_ids: HashMap::new(),
            tx: OnceCell::new(),
        }
    }

    pub async fn load(api_config: Config) -> Result<Nodes> {
        // First load the common data file.
        info!(
            "Reading nodes common config file: {}",
            REGISTRY_CONFIG_FILE.display()
        );
        let config = fs::read_to_string(&*REGISTRY_CONFIG_FILE)
            .await
            .context("failed to read nodes registry")?;
        let nodes_data = toml::from_str(&config).context("failed to parse nodes registry")?;
        // Now the individual node data files.
        info!(
            "Reading nodes config dir: {}",
            REGISTRY_CONFIG_DIR.display()
        );
        let mut this = Nodes::new(api_config, nodes_data);
        let mut dir = read_dir(&*REGISTRY_CONFIG_DIR)
            .await
            .context("failed to read nodes registry dir")?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .context("failed to read nodes registry entry")?
        {
            // blockvisord should not bail on problems with individual node files.
            // It should log warnings though.
            let path = entry.path();
            if path == *REGISTRY_CONFIG_FILE {
                // Skip the common data file.
                continue;
            }
            match NodeData::load(&path)
                .and_then(|data| async { Node::connect(data).await })
                .await
            {
                Ok(node) => {
                    this.node_ids.insert(node.data.name.clone(), node.id());
                    this.nodes.insert(node.data.id, node);
                }
                Err(e) => {
                    warn!(
                        "Failed to connect to node from file `{}`: {}",
                        path.display(),
                        e
                    );
                }
            };
        }

        Ok(this)
    }

    pub async fn save(&self) -> Result<()> {
        // We only save the common data file. The individual node data files save themselves.
        info!(
            "Writing nodes common config file: {}",
            REGISTRY_CONFIG_FILE.display()
        );
        let config = toml::Value::try_from(&self.data)?;
        let config = toml::to_string(&config)?;
        fs::create_dir_all(REGISTRY_CONFIG_DIR.as_path()).await?;
        fs::write(&*REGISTRY_CONFIG_FILE, &*config).await?;

        Ok(())
    }

    pub fn exists() -> bool {
        Path::new(&*REGISTRY_CONFIG_FILE).exists()
    }

    pub fn send_container_status(&self, id: &Uuid, status: ContainerStatus) -> Result<()> {
        if !self.tx.initialized() {
            bail!("Updates channel not initialized")
        }

        let node_id = id.to_string();
        let update = pb::InfoUpdate {
            info: Some(pb::info_update::Info::Node(pb::NodeInfo {
                id: node_id,
                container_status: Some(status.into()),
                ..Default::default()
            })),
        };

        match self.tx.get().unwrap().send(update) {
            Ok(_) => Ok(()),
            Err(error) => {
                let msg = format!("Cannot send node status: {error:?}");
                error!(msg);
                Err(anyhow!(msg))
            }
        }
    }

    /// Create and return the next network interface using machine index
    pub async fn create_network_interface(
        &mut self,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<NetworkInterface> {
        self.data.machine_index += 1;

        let iface = NetworkInterface::create(format!("bv{}", self.data.machine_index), ip, gateway)
            .await
            .context(format!(
                "failed to create VM bridge bv{}",
                self.data.machine_index
            ))?;

        Ok(iface)
    }

    // Get or init updates sender
    pub async fn get_updates_sender(&self) -> Result<&Sender<pb::InfoUpdate>> {
        self.tx
            .get_or_try_init(|| async {
                let (tx, _rx) = broadcast::channel(128);
                Ok(tx)
            })
            .await
    }
}

pub fn check_babel_version(min_babel_version: &str) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    if version < min_babel_version {
        bail!(format!(
            "Required minimum babel version is `{}`, running is `{}`",
            min_babel_version, version
        ));
    }
    Ok(())
}
