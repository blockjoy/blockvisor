use anyhow::{anyhow, bail, Context, Result};
use babel_api::config::{Babel, Entrypoint};
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{self, read_dir};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::node::build_registry_dir;
use crate::pal::Pal;
use crate::{
    config::Config,
    node::Node,
    node_data::{NodeData, NodeImage, NodeProperties, NodeStatus},
    render,
    services::{
        api::{self, pb, pb::node_info::ContainerStatus, pb::Parameter},
        cookbook::CookbookService,
        keyfiles::KeyService,
    },
    BV_VAR_PATH,
};

pub const REGISTRY_CONFIG_FILENAME: &str = "nodes.toml";

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

#[derive(Debug)]
pub struct Nodes<P: Pal + Debug> {
    pub api_config: Config,
    pub nodes: HashMap<Uuid, Node<P>>,
    pub node_ids: HashMap<String, Uuid>,
    data: CommonData,
    pal: Arc<P>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CommonData {
    pub machine_index: u32,
}

impl<P: Pal + Debug> Nodes<P> {
    #[instrument(skip(self))]
    pub async fn create(
        &mut self,
        id: Uuid,
        name: String,
        image: NodeImage,
        ip: String,
        gateway: String,
        properties: NodeProperties,
    ) -> Result<()> {
        if self.nodes.contains_key(&id) {
            warn!("Node with id `{id}` exists");
            return Ok(());
        }

        if self.node_ids.contains_key(&name) {
            bail!("Node with name `{name}` exists");
        }

        let mut babel_conf = self.fetch_image_data(&image).await?;
        check_babel_version(&babel_conf.config.min_babel_version)?;
        let conf = toml::Value::try_from(&babel_conf)?;
        babel_conf.supervisor.entry_point =
            render_entry_points(babel_conf.supervisor.entry_point, &properties, &conf)?;

        self.send_container_status(id, ContainerStatus::Creating)
            .await;

        self.data.machine_index += 1;
        let ip = ip.parse()?;
        let gateway = gateway.parse()?;
        let network_interface = self.create_network_interface(ip, gateway).await?;

        let node_data = NodeData {
            id,
            name: name.clone(),
            image,
            expected_status: NodeStatus::Stopped,
            network_interface,
            babel_conf,
            self_update: false,
            properties,
        };
        self.save().await?;

        let node = Node::create(self.pal.clone(), node_data).await?;
        self.nodes.insert(id, node);
        self.node_ids.insert(name, id);
        debug!("Node with id `{}` created", id);

        self.send_container_status(id, ContainerStatus::Stopped)
            .await;

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn upgrade(&mut self, id: Uuid, image: NodeImage) -> Result<()> {
        if image
            != self
                .nodes
                .get(&id)
                .ok_or_else(|| id_not_found(id))?
                .data
                .image
        {
            let babel = self.fetch_image_data(&image).await?;
            check_babel_version(&babel.config.min_babel_version)?;

            self.send_container_status(id, ContainerStatus::Upgrading)
                .await;

            let need_to_restart = self.status(id).await? == NodeStatus::Running;
            self.stop(id).await?;
            let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(id))?;

            if image.protocol != node.data.image.protocol {
                bail!("Cannot upgrade protocol to `{}`", image.protocol);
            }
            if image.node_type != node.data.image.node_type {
                bail!("Cannot upgrade node type to `{}`", image.node_type);
            }
            if node.data.babel_conf.requirements.vcpu_count != babel.requirements.vcpu_count
                || node.data.babel_conf.requirements.mem_size_mb != babel.requirements.mem_size_mb
                || node.data.babel_conf.requirements.disk_size_gb != babel.requirements.disk_size_gb
            {
                bail!("Cannot upgrade node requirements");
            }

            node.upgrade(&image).await?;
            debug!("Node upgraded");

            if need_to_restart {
                self.start(id).await?;
            }

            self.send_container_status(id, ContainerStatus::Upgraded)
                .await;
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn fetch_image_data(&mut self, image: &NodeImage) -> Result<Babel> {
        if !CookbookService::is_image_cache_valid(self.pal.bv_root(), image)
            .await
            .with_context(|| format!("Failed to check image cache: `{image:?}`"))?
        {
            let mut cookbook_service = CookbookService::connect(
                self.pal.bv_root().to_path_buf(),
                &self.api_config.blockjoy_registry_url,
                &self.api_config.token,
            )
            .await?;

            cookbook_service.download_babel_config(image).await?;
            cookbook_service.download_image(image).await?;
            cookbook_service.download_kernel(image).await?;
        }

        let babel = CookbookService::get_babel_config(self.pal.bv_root(), image).await?;
        Ok(babel)
    }

    #[instrument(skip(self))]
    pub async fn delete(&mut self, id: Uuid) -> Result<()> {
        if let Some(node) = self.nodes.remove(&id) {
            self.node_ids.remove(&node.data.name);
            node.delete().await?;
            debug!("Node deleted");
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn force_start(&mut self, id: Uuid) -> Result<()> {
        self.send_container_status(id, ContainerStatus::Starting)
            .await;

        let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(id))?;
        node.start().await?;
        debug!("Node started");

        self.send_container_status(id, ContainerStatus::Running)
            .await;

        let secret_keys = match self.exchange_keys(id).await {
            Ok(secret_keys) => secret_keys,
            Err(e) => {
                error!("Failed to retrieve keys when starting node: `{e}`");
                HashMap::new()
            }
        };

        let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(id))?;
        node.init(secret_keys).await?;

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn start(&mut self, id: Uuid) -> Result<()> {
        if NodeStatus::Running
            != self
                .nodes
                .get(&id)
                .ok_or_else(|| id_not_found(id))?
                .expected_status()
        {
            self.force_start(id).await
        } else {
            Ok(())
        }
    }

    #[instrument(skip(self))]
    pub async fn update(
        &mut self,
        id: Uuid,
        name: Option<String>,
        self_update: Option<bool>,
        properties: Vec<Parameter>,
    ) -> Result<()> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or_else(|| anyhow!("No node exists with id `{id}`"))?;
        node.update(name, self_update, properties).await
    }

    #[instrument(skip(self))]
    pub async fn force_stop(&mut self, id: Uuid) -> Result<()> {
        self.send_container_status(id, ContainerStatus::Stopping)
            .await;

        let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(id))?;
        node.stop().await?;
        debug!("Node stopped");

        self.send_container_status(id, ContainerStatus::Stopped)
            .await;
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn stop(&mut self, id: Uuid) -> Result<()> {
        if NodeStatus::Stopped
            != self
                .nodes
                .get(&id)
                .ok_or_else(|| id_not_found(id))?
                .expected_status()
        {
            self.force_stop(id).await
        } else {
            Ok(())
        }
    }

    #[instrument(skip(self))]
    pub async fn list(&self) -> Vec<&Node<P>> {
        debug!("Listing {} nodes", self.nodes.len());

        self.nodes.values().collect()
    }

    #[instrument(skip(self))]
    pub async fn status(&self, id: Uuid) -> Result<NodeStatus> {
        let node = self.nodes.get(&id).ok_or_else(|| id_not_found(id))?;

        Ok(node.status())
    }

    pub async fn node_id_for_name(&self, name: &str) -> Result<Uuid> {
        let uuid = self
            .node_ids
            .get(name)
            .copied()
            .ok_or_else(|| name_not_found(name))?;

        Ok(uuid)
    }

    /// Synchronizes the keys in the key server with the keys available locally. Returns a
    /// refreshed set of all keys.
    pub async fn exchange_keys(&mut self, id: Uuid) -> Result<HashMap<String, Vec<u8>>> {
        let node = self.nodes.get_mut(&id).ok_or_else(|| id_not_found(id))?;

        let mut key_service =
            KeyService::connect(&self.api_config.blockjoy_keys_url, &self.api_config.token).await?;

        let api_keys: HashMap<String, Vec<u8>> = key_service
            .download_keys(id)
            .await?
            .into_iter()
            .map(|k| (k.name, k.content))
            .collect();
        let api_keys_set: HashSet<&String> = HashSet::from_iter(api_keys.keys());
        debug!("Received API keys: {api_keys_set:?}");

        let node_keys: HashMap<String, Vec<u8>> = node
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
            && node.has_capability(&babel_api::BabelMethod::GenerateKeys.to_string())
        {
            node.generate_keys().await?;
            let gen_keys: Vec<_> = node
                .download_keys()
                .await?
                .into_iter()
                .map(|k| pb::Keyfile {
                    name: k.name,
                    content: k.content,
                })
                .collect();
            key_service.upload_keys(id, gen_keys.clone()).await?;
            return Ok(gen_keys.into_iter().map(|k| (k.name, k.content)).collect());
        }

        let all_keys = api_keys.into_iter().chain(node_keys.into_iter()).collect();
        Ok(all_keys)
    }

    pub async fn load(pal: P, api_config: Config) -> Result<Self> {
        let bv_root = pal.bv_root();
        let registry_dir = build_registry_dir(bv_root);
        if !registry_dir.exists() {
            fs::create_dir_all(&registry_dir).await?;
        }
        let registry_path = build_registry_filename(bv_root);
        let pal = Arc::new(pal);
        Ok(if registry_path.exists() {
            let data = Self::load_data(&registry_path).await?;
            let (nodes, node_ids) = Self::load_nodes(pal.clone(), &registry_dir).await?;

            Self {
                api_config,
                data,
                nodes,
                node_ids,
                pal,
            }
        } else {
            let nodes = Self {
                api_config,
                data: CommonData { machine_index: 0 },
                nodes: Default::default(),
                node_ids: Default::default(),
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
        toml::from_str(&config).context("failed to parse nodes registry")
    }

    async fn load_nodes(
        pal: Arc<P>,
        registry_dir: &Path,
    ) -> Result<(HashMap<Uuid, Node<P>>, HashMap<String, Uuid>)> {
        info!("Reading nodes config dir: {}", registry_dir.display());
        let mut nodes = HashMap::new();
        let mut node_ids = HashMap::new();
        let mut dir = read_dir(registry_dir)
            .await
            .context("failed to read nodes registry dir")?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .context("failed to read nodes registry entry")?
        {
            let path = entry.path();
            match NodeData::load(&path)
                .and_then(|data| async { Node::attach(pal.clone(), data).await })
                .await
            {
                Ok(node) => {
                    node_ids.insert(node.data.name.clone(), node.id());
                    nodes.insert(node.data.id, node);
                }
                Err(e) => {
                    // blockvisord should not bail on problems with individual node files.
                    // It should log error though.
                    error!("Failed to load node from file `{}`: {}", path.display(), e);
                }
            };
        }
        Ok((nodes, node_ids))
    }

    pub async fn save(&self) -> Result<()> {
        let registry_path = build_registry_filename(self.pal.bv_root());
        // We only save the common data file. The individual node data files save themselves.
        info!(
            "Writing nodes common config file: {}",
            registry_path.display()
        );
        let config = toml::Value::try_from(&self.data)?;
        let config = toml::to_string(&config)?;
        fs::write(&*registry_path, &*config).await?;

        Ok(())
    }

    // Optimistically try to notify API that container is 'Running' or 'Stopped', etc
    pub async fn send_container_status(&self, id: Uuid, status: ContainerStatus) {
        let update = pb::NodeInfo {
            id: id.to_string(),
            container_status: Some(status.into()),
            ..Default::default()
        };
        if let Err(e) = self.send_info_update(update).await {
            error!("Cannot send container status: {e}");
        };
    }

    // Send node info update to API
    pub async fn send_info_update(&self, update: pb::NodeInfo) -> Result<()> {
        let mut client =
            api::NodesService::connect(&self.api_config.blockjoy_api_url, &self.api_config.token)
                .await
                .with_context(|| "Error connecting to api".to_string())?;
        client
            .send_node_update(update)
            .await
            .with_context(|| "Cannot send node update".to_string())?;
        Ok(())
    }

    /// Create and return the next network interface using machine index
    async fn create_network_interface(
        &mut self,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<<P as Pal>::NetInterface> {
        self.data.machine_index += 1;

        let iface = self
            .pal
            .create_net_interface(self.data.machine_index, ip, gateway)
            .await
            .context(format!(
                "failed to create VM bridge bv{}",
                self.data.machine_index
            ))?;

        Ok(iface)
    }
}

fn check_babel_version(min_babel_version: &str) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    if version < min_babel_version {
        bail!("Required minimum babel version is `{min_babel_version}`, running is `{version}`");
    }
    Ok(())
}

fn render_entry_points(
    mut entry_points: Vec<Entrypoint>,
    params: &NodeProperties,
    conf: &toml::Value,
) -> Result<Vec<Entrypoint>> {
    let params = params
        .iter()
        .map(|(k, v)| (k.clone(), vec![v.clone()]))
        .collect();
    for item in &mut entry_points {
        item.body = render::render_with(&item.body, &params, conf, render::render_sh_param)?;
    }
    Ok(entry_points)
}

#[cfg(test)]
mod tests {
    use crate::{linux_platform, utils};

    use super::*;
    use babel_api::config::{Method, MethodResponseFormat, Requirements, ShResponse};
    use std::collections::BTreeMap;
    use std::str::FromStr;

    #[test]
    fn test_render_entry_point_args() -> Result<()> {
        let conf = toml::toml!(
        [aa]
        bb = "cc"
        );
        let entrypoints = vec![
            Entrypoint {
                name: "cmd1".to_string(),
                body:"first_parametrized_{{PARAM1}}_argument second_parametrized_{{PARAM1}}_{{PARAM2}}_argument third_parammy_babelref:'aa.{{PARAM_BB}}'_argument".to_string(),
            },
            Entrypoint {
                name: "cmd2".to_owned(),
                body: "{{PARAM1}} and {{PARAM2}} twice {{PARAM1}} and none{a}".to_owned(),
            },
        ];
        let node_props = HashMap::from([
            ("PARAM1".to_string(), "://Value.1,-_Q".to_string()),
            ("PARAM2".to_string(), "Value.2,-_Q".to_string()),
            ("PARAM3".to_string(), "!Invalid_but_not_used".to_string()),
            ("PARAM_BB".to_string(), "bb".to_string()),
        ]);
        assert_eq!(
            vec![
                Entrypoint {
                    name: "cmd1".to_string(),
                    body: r#"first_parametrized_"://Value.1,-_Q"_argument second_parametrized_"://Value.1,-_Q"_"Value.2,-_Q"_argument third_parammy_cc_argument"#.to_string(),
                },
                Entrypoint {
                    name: "cmd2".to_owned(),
                    body: r#""://Value.1,-_Q" and "Value.2,-_Q" twice "://Value.1,-_Q" and none{a}"#.to_owned(),
                }
            ],
            render_entry_points(entrypoints.clone(), &node_props, &conf)?
        );

        let mut invalid_props = node_props.clone();
        invalid_props.get_mut("PARAM1").unwrap().push('@');
        render_entry_points(entrypoints.clone(), &invalid_props, &conf).unwrap_err();

        let mut missing_props = node_props;
        missing_props.remove("PARAM1");
        render_entry_points(entrypoints, &missing_props, &conf).unwrap_err();

        Ok(())
    }

    #[tokio::test]
    async fn test_smoke_node_data_serialization() -> Result<()> {
        let babel_conf = Babel {
            export: None,
            env: None,
            config: utils::tests::default_config(),
            requirements: Requirements {
                vcpu_count: 0,
                mem_size_mb: 0,
                disk_size_gb: 0,
            },
            nets: Default::default(),
            supervisor: Default::default(),
            keys: None,
            methods: BTreeMap::from([
                (
                    "raw".to_string(),
                    Method::Sh {
                        name: "raw".to_string(),
                        body: "echo make a toast".to_string(),
                        response: ShResponse {
                            status: 101,
                            format: MethodResponseFormat::Raw,
                        },
                    },
                ),
                (
                    "json".to_string(),
                    Method::Sh {
                        name: "json".to_string(),
                        body: "echo \\\"make a toast\\\"".to_string(),
                        response: ShResponse {
                            status: 102,
                            format: MethodResponseFormat::Json,
                        },
                    },
                ),
            ]),
        };

        let node = NodeData {
            id: Uuid::new_v4(),
            name: "name".to_string(),
            image: NodeImage {
                protocol: "".to_string(),
                node_type: "".to_string(),
                node_version: "".to_string(),
            },
            expected_status: NodeStatus::Stopped,
            network_interface: linux_platform::LinuxNetInterface {
                name: "".to_string(),
                ip: IpAddr::from_str("1.1.1.1")?,
                gateway: IpAddr::from_str("1.1.1.1")?,
            },
            babel_conf,
            self_update: false,
            properties: HashMap::from([
                ("raw".to_string(), "raw".to_string()),
                ("json".to_string(), "json".to_string()),
            ]),
        };

        let serialized = toml::to_string(&node)?;
        let _deserialized: NodeData<linux_platform::LinuxNetInterface> =
            toml::from_str(&serialized)?;
        Ok(())
    }
}
