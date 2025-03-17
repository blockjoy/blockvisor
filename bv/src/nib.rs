use crate::{
    apptainer_machine::{self, PLUGIN_MAIN_FILENAME, PLUGIN_PATH},
    bv_config, firewall,
    internal_server::{self, service_client::ServiceClient, NodeDisplayInfo},
    nib_cli::{ImageCommand, NodeChecks, ProtocolCommand},
    nib_meta::{
        self, ArchivePointer, FirewallConfig, ImageProperty, RamdiskConfig, Variant,
        VariantMetadata, Visibility,
    },
    node_context,
    node_state::{NodeImage, NodeProperties, NodeState, ProtocolImageKey, VmConfig, VmStatus},
    services::{self, protocol::PushResult, ApiServiceConnector},
    utils,
};
use babel_api::{engine::NodeEnv, rhai_plugin_linter, utils::RamdiskConfiguration};
use bv_utils::cmd::run_cmd;
use eyre::{anyhow, bail, ensure, Context};
use petname::{Generator, Petnames};
use serde::Serialize;
use std::{
    collections::HashMap,
    ffi::OsStr,
    net,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};
use tokio::fs;
use tokio::time::sleep;
use tonic::transport::{Channel, Endpoint};
use tracing::info;
use uuid::Uuid;

const BV_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BV_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const VARIANT_METADATA_NETWORK_KEY: &str = "network";
const VARIANT_METADATA_CLIENT_KEY: &str = "client";
const VARIANT_METADATA_NODE_TYPE_KEY: &str = "node-type";

pub async fn process_image_command(
    connector: impl ApiServiceConnector + Clone,
    bv_root: &Path,
    command: ImageCommand,
) -> eyre::Result<()> {
    match command {
        ImageCommand::GenerateMapping => {
            let mut mapping: HashMap<String, (String, ProtocolImageKey)> = Default::default();
            for entry in walkdir::WalkDir::new(std::env::current_dir()?) {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if let Some(file_name) = path.file_name() {
                    let file_name = file_name.to_string_lossy();
                    if file_name.starts_with("babel") && file_name.ends_with("yaml") {
                        if let Ok(image) = serde_yaml_ng::from_str::<nib_meta::Image>(
                            &fs::read_to_string(path).await?,
                        ) {
                            let protocol_key = image.protocol_key;
                            for variant in image.variants {
                                let variant_key = variant.key;
                                for pointer in variant.archive_pointers {
                                    let nib_meta::StorePointer::StoreKey(store_key) =
                                        pointer.pointer
                                    else {
                                        continue;
                                    };
                                    let Some(legacy_store_key) = pointer.legacy_store_key else {
                                        continue;
                                    };
                                    if let Some((
                                        _,
                                        ProtocolImageKey {
                                            protocol_key: first_protocol_key,
                                            variant_key: first_variant_key,
                                        },
                                    )) = mapping.get(&legacy_store_key)
                                    {
                                        bail!("legacy_store_key '{legacy_store_key}' defined twice: first for {first_protocol_key}/{first_variant_key}, then for {protocol_key}/{variant_key}");
                                    }
                                    mapping.insert(
                                        legacy_store_key,
                                        (
                                            store_key,
                                            ProtocolImageKey {
                                                protocol_key: protocol_key.clone(),
                                                variant_key: variant_key.clone(),
                                            },
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            println!("{}", serde_json::to_string(&mapping)?);
        }
        ImageCommand::Create { protocol, variant } => {
            let mut parts = variant.rsplitn(3, "-");
            let node_type = parts.next().unwrap_or("Node Type");
            let network = parts.next().unwrap_or("Network Name");
            let client = parts.next().unwrap_or("Protocol Client Name");
            let params = [
                ("protocol_key", protocol.as_str()),
                ("variant_key", variant.as_str()),
                ("client", client),
                ("network", network),
                ("node_type", node_type),
            ];
            let image_path = std::env::current_dir()?.join(format!("{}_{}", protocol, variant));
            fs::create_dir_all(&image_path).await?;
            let babel_file_path = image_path.join("babel.yaml");
            println!("Render babel file at `{}`", babel_file_path.display());
            utils::render_template(
                include_str!("../data/babel.yaml.template"),
                &babel_file_path,
                &params,
            )?;
            let plugin_file_path = image_path.join(PLUGIN_MAIN_FILENAME);
            println!("Render plugin file at `{}`", plugin_file_path.display());
            utils::render_template(
                include_str!("../data/main.rhai.template"),
                &plugin_file_path,
                &params,
            )?;
            let dockerfile_path = image_path.join("Dockerfile");
            println!("Render dockerfile at `{}`", dockerfile_path.display());
            utils::render_template(
                include_str!("../data/Dockerfile.template"),
                &dockerfile_path,
                &params,
            )?;
        }
        ImageCommand::ContainerUri { path } => {
            let local_image: nib_meta::Image =
                serde_yaml_ng::from_str(&fs::read_to_string(path).await?)?;
            println!("{}", local_image.container_uri);
        }
        ImageCommand::Play {
            ip,
            gateway,
            props,
            tags,
            path,
            variant,
        } => {
            let image: nib_meta::Image = serde_yaml_ng::from_str(&fs::read_to_string(path).await?)?;
            let variant = pick_variant(image.variants.clone(), variant)?;
            let image_variant = ImageVariant::build(&image, variant);
            let properties = build_properties(&image_variant.properties, props)?;

            let node_info = DevNode::create(bv_root, ip, gateway, image_variant, properties, tags)
                .await?
                .node_info;
            println!(
                "Created new dev_node with ID `{}` and name `{}`\n{:#?}",
                node_info.state.id, node_info.state.name, node_info.state
            );
        }
        ImageCommand::Upgrade { path, id_or_name } => {
            let bv_config = load_bv_config(bv_root).await?;
            let bv_url = format!("http://localhost:{}", bv_config.blockvisor_port);
            let mut bv_client = internal_server::service_client::ServiceClient::connect(
                Endpoint::from_shared(bv_url)?
                    .connect_timeout(BV_CONNECT_TIMEOUT)
                    .timeout(BV_REQUEST_TIMEOUT),
            )
            .await?;
            let id = match Uuid::parse_str(&id_or_name) {
                Ok(v) => v,
                Err(_) => {
                    let id = bv_client
                        .get_node_id_for_name(id_or_name)
                        .await?
                        .into_inner();
                    Uuid::parse_str(&id)?
                }
            };
            let image: nib_meta::Image = serde_yaml_ng::from_str(&fs::read_to_string(path).await?)?;
            let mut node = bv_client.get_node(id).await?.into_inner().state;
            let variant = image
                .variants
                .iter()
                .find(|variant| variant.key == node.image_key.variant_key)
                .ok_or(anyhow!(
                    "variant {} not found in image definition",
                    node.image_key.variant_key
                ))?;
            let image_variant = ImageVariant::build(&image, variant.clone());
            node.vm_config = VmConfig::build_from(&image_variant);
            node.firewall = image_variant.firewall_config.into();
            node.image.id = format!("dev-node-image-id-{}", image_variant.version);
            node.image.version = image_variant.version;
            node.image.uri = image_variant.container_uri;
            let node = bv_client.upgrade_dev_node(node).await?.into_inner();
            println!(
                "Upgraded dev_node with ID `{}` and name `{}`\n{:#?}",
                node.state.id, node.state.name, node.state
            );
        }
        ImageCommand::Check {
            ip,
            gateway,
            props,
            path,
            variant,
            start_timeout,
            jobs_wait,
            cleanup,
            force_cleanup,
            tags,
            checks,
        } => {
            let image: nib_meta::Image =
                serde_yaml_ng::from_str(&fs::read_to_string(&path).await?)?;
            let variant = pick_variant(image.variants.clone(), variant)?;
            let image_variant = ImageVariant::build(&image, variant.clone());
            utils::verify_variant_sku(&image_variant)?;
            let properties = build_properties(&image_variant.properties, props)?;
            let checks = checks.unwrap_or(vec![NodeChecks::Plugin, NodeChecks::JobsStatus]);
            let tmp_dir = tempdir::TempDir::new("nib_check")?;
            let (rootfs_path, dev_node) =
                if checks.len() == 1 && checks.contains(&NodeChecks::Plugin) {
                    let rootfs_path = tmp_dir.path();
                    run_cmd(
                        "apptainer",
                        [
                            OsStr::new("build"),
                            OsStr::new("--sandbox"),
                            OsStr::new("--force"),
                            rootfs_path.as_os_str(),
                            OsStr::new(&image_variant.container_uri),
                        ],
                    )
                    .await
                    .map_err(|err| {
                        anyhow!(
                            "failed to build sandbox for '{}' in `{}`: {err:#}",
                            path.display(),
                            rootfs_path.display(),
                        )
                    })?;
                    (rootfs_path.to_path_buf(), None)
                } else {
                    let dev_node = DevNode::create(
                        bv_root,
                        ip,
                        gateway,
                        image_variant.clone(),
                        properties.clone(),
                        tags,
                    )
                    .await?;
                    println!(
                        "Created '{}' dev_node with ID `{}`",
                        dev_node.node_info.state.name, dev_node.node_info.state.id
                    );
                    (
                        apptainer_machine::build_rootfs_dir(&node_context::build_node_dir(
                            bv_root,
                            dev_node.node_info.state.id,
                        )),
                        Some(dev_node),
                    )
                };

            println!("Checking plugin");
            let mut res = rhai_plugin_linter::check(
                rootfs_path.join(PLUGIN_PATH).join(PLUGIN_MAIN_FILENAME),
                NodeEnv {
                    node_id: "node-id".to_string(),
                    node_name: "node_name".to_string(),
                    node_version: image_variant.version,
                    node_protocol: image_variant.protocol_key,
                    node_variant: variant.key,
                    node_ip: "1.2.3.4".to_string(),
                    node_gateway: "4.3.2.1".to_string(),
                    dev_mode: true,
                    bv_host_id: "host-id".to_string(),
                    bv_host_name: "hostname".to_string(),
                    bv_api_url: "none.com".to_string(),
                    node_org_id: "org-id".to_string(),
                    data_mount_point: PathBuf::from("/blockjoy"),
                    protocol_data_path: PathBuf::from("/blockjoy/protocol_data"),
                },
                properties,
            );
            if let Some(mut dev_node) = dev_node {
                if res.is_ok() {
                    res = dev_node.run_checks(start_timeout, jobs_wait, checks).await;
                }
                if force_cleanup || (cleanup && res.is_ok()) {
                    if let Err(err) = dev_node.delete().await {
                        println!("Failed to delete test node: {err:#}");
                    } else {
                        println!("Node deleted");
                    }
                }
            }
            res?;
            println!("All checks passed!");
        }
        ImageCommand::Push {
            min_babel_version,
            path,
        } => {
            let min_babel_version =
                min_babel_version.unwrap_or(env!("CARGO_PKG_VERSION").to_string());
            let mut client = services::protocol::ProtocolService::new(connector).await?;
            let image: nib_meta::Image = serde_yaml_ng::from_str(&fs::read_to_string(path).await?)?;
            let image_variants: Vec<_> = image
                .variants
                .iter()
                .map(|variant| ImageVariant::build(&image, variant.clone()))
                .map(|image_variant| {
                    utils::verify_variant_sku(&image_variant).map(|_| image_variant)
                })
                .collect::<eyre::Result<_>>()?;
            for image_variant in image_variants {
                let image_key = ProtocolImageKey {
                    protocol_key: image_variant.protocol_key.clone(),
                    variant_key: image_variant.variant_key.clone(),
                };

                let protocol_version_id =
                    match client.get_protocol_version(image_key.clone()).await? {
                        Some(remote) if remote.semantic_version == image_variant.version => {
                            client
                                .update_protocol_version(remote.clone(), image_variant.clone())
                                .await?;
                            println!(
                                "Variant version '{}/{}/{}' updated",
                                image_variant.protocol_key,
                                image_variant.variant_key,
                                image_variant.version
                            );
                            remote.protocol_version_id
                        }
                        _ => {
                            let protocol_version_id = client
                                .add_protocol_version(image_variant.clone())
                                .await?
                                .protocol_version_id;
                            println!(
                                "Variant version '{}/{}/{}' added",
                                image_variant.protocol_key,
                                image_variant.variant_key,
                                image_variant.version
                            );
                            protocol_version_id
                        }
                    };
                match client
                    .push_image(
                        protocol_version_id,
                        image_variant.clone(),
                        min_babel_version.clone(),
                    )
                    .await?
                {
                    PushResult::Added(image) => println!(
                        "Image '{}/{}/{}/{}' added",
                        image_variant.protocol_key,
                        image_variant.variant_key,
                        image_variant.version,
                        image.build_version,
                    ),
                    PushResult::Updated(image) => println!(
                        "Image '{}/{}/{}/{}' updated",
                        image_variant.protocol_key,
                        image_variant.variant_key,
                        image_variant.version,
                        image.build_version,
                    ),
                    PushResult::NoChanges => println!("No image changes to push"),
                }
            }
        }
    }
    Ok(())
}

pub async fn process_protocol_command(
    connector: impl ApiServiceConnector + Clone,
    command: ProtocolCommand,
) -> eyre::Result<()> {
    let mut client = services::protocol::ProtocolService::new(connector).await?;
    match command {
        ProtocolCommand::List { name, number } => {
            for protocol in client.list_protocols(name, number).await? {
                println!("{protocol}");
            }
        }
        ProtocolCommand::Push { path } => {
            let local_protocols: Vec<nib_meta::Protocol> =
                serde_yaml_ng::from_str(&fs::read_to_string(path).await?)?;
            for local in local_protocols {
                let protocol_key = local.key.clone();

                if let Some(remote) = client.get_protocol(protocol_key.clone()).await? {
                    client
                        .update_protocol(remote.protocol_id.clone(), local)
                        .await?;
                    println!("Protocol '{protocol_key}' updated");
                } else {
                    client.add_protocol(local).await?;
                    println!("Protocol '{protocol_key}' added");
                }
            }
        }
    }
    Ok(())
}

pub async fn load_bv_config(bv_root: &Path) -> eyre::Result<bv_config::Config> {
    let bv_path = bv_root.join(bv_config::CONFIG_PATH);
    Ok(serde_json::from_str(&fs::read_to_string(&bv_path).await?)?)
}

async fn discover_ip_and_gateway(
    config: &bv_config::Config,
    ip: Option<String>,
    gateway: Option<String>,
    nodes: &[NodeDisplayInfo],
    id: Uuid,
) -> eyre::Result<(net::IpAddr, net::IpAddr)> {
    let gateway = match &gateway {
        None => config.net_conf.gateway_ip,
        Some(gateway) => net::IpAddr::from_str(gateway)
            .with_context(|| format!("invalid gateway `{gateway}`"))?,
    };
    let ip = match &ip {
        None => {
            let used_ips = nodes.iter().map(|node| node.state.ip).collect::<Vec<_>>();
            let ip = *config
                .net_conf
                .available_ips
                .iter()
                .find(|ip| !used_ips.contains(ip))
                .ok_or(anyhow!("failed to auto assign ip - provide it manually"))?;
            info!("Auto-assigned ip `{ip}` for node '{id}'");
            ip
        }
        Some(ip) => net::IpAddr::from_str(ip).with_context(|| format!("invalid ip `{ip}`"))?,
    };
    Ok((ip, gateway))
}

fn pick_variant(variants: Vec<Variant>, variant_key: Option<String>) -> eyre::Result<Variant> {
    if let Some(variant_key) = variant_key {
        variants
            .into_iter()
            .find(|variant| variant.key == variant_key)
            .ok_or(anyhow!("variant '{variant_key}' not found"))
    } else {
        let mut iter = variants.into_iter();
        if let Some(first) = iter.next() {
            if iter.next().is_some() {
                bail!("multiple variants found, please choose one");
            }
            Ok(first)
        } else {
            bail!("no image variant defined");
        }
    }
}

fn build_properties(
    image_properties: &[ImageProperty],
    overrides: Option<String>,
) -> eyre::Result<HashMap<String, String>> {
    let mut properties: HashMap<_, _> = image_properties
        .iter()
        .map(|property| (property.key.clone(), property.default_value.clone()))
        .collect();
    if let Some(props) = overrides {
        for (key, value) in serde_yaml_ng::from_str::<HashMap<_, _>>(&props)? {
            properties.insert(key, value);
        }
    }
    Ok(properties)
}

#[derive(Clone, Debug, Serialize)]
pub struct ImageVariant {
    pub version: String,
    pub container_uri: String,
    pub protocol_key: String,
    pub org_id: Option<String>,
    pub description: Option<String>,
    pub visibility: Visibility,
    pub properties: Vec<ImageProperty>,
    pub firewall_config: FirewallConfig,
    pub variant_key: String,
    pub sku_code: String,
    pub archive_pointers: Vec<ArchivePointer>,
    pub min_cpu: u64,
    pub min_memory_mb: u64,
    pub min_disk_gb: u64,
    pub ramdisks: Vec<RamdiskConfig>,
    pub metadata: Vec<VariantMetadata>,
    pub dns_scheme: Option<String>,
}

impl ImageVariant {
    fn build(image: &nib_meta::Image, variant: Variant) -> Self {
        let mut parts = variant.key.rsplitn(3, "-");
        let mut metadata = variant.metadata.unwrap_or_default();
        let mut default_meta = |part: Option<&str>, key: &str| {
            if let Some(value) = part {
                if !metadata.iter().any(|item| item.key == key) {
                    metadata.push(VariantMetadata {
                        key: key.to_string(),
                        value: value.to_string(),
                        description: None,
                    });
                }
            }
        };
        default_meta(parts.next(), VARIANT_METADATA_NODE_TYPE_KEY);
        default_meta(parts.next(), VARIANT_METADATA_NETWORK_KEY);
        default_meta(parts.next(), VARIANT_METADATA_CLIENT_KEY);
        Self {
            version: image.version.clone(),
            container_uri: image.container_uri.clone(),
            protocol_key: image.protocol_key.clone(),
            org_id: image.org_id.clone(),
            description: variant.description.or(image.description.clone()),
            visibility: variant.visibility.unwrap_or(image.visibility.clone()),
            properties: variant.properties.unwrap_or(image.properties.clone()),
            firewall_config: variant
                .firewall_config
                .unwrap_or(image.firewall_config.clone()),
            variant_key: variant.key,
            sku_code: variant.sku_code,
            archive_pointers: variant.archive_pointers,
            min_cpu: variant.min_cpu,
            min_memory_mb: variant.min_memory_mb,
            min_disk_gb: variant.min_disk_gb,
            ramdisks: variant.ramdisks,
            metadata,
            dns_scheme: variant.dns_scheme.or(image.dns_scheme.clone()),
        }
    }
}

struct DevNode {
    node_info: NodeDisplayInfo,
    bv_client: ServiceClient<Channel>,
}

impl DevNode {
    async fn create(
        bv_root: &Path,
        ip: Option<String>,
        gateway: Option<String>,
        image_variant: ImageVariant,
        properties: NodeProperties,
        tags: Vec<String>,
    ) -> eyre::Result<Self> {
        let bv_config = load_bv_config(bv_root).await?;
        let bv_url = format!("http://localhost:{}", bv_config.blockvisor_port);
        let mut bv_client = internal_server::service_client::ServiceClient::connect(
            Endpoint::from_shared(bv_url)?.connect_timeout(BV_CONNECT_TIMEOUT),
        )
        .await?;
        let nodes = bv_client.get_nodes(true).await?.into_inner();
        let id = Uuid::new_v4();
        let (ip, gateway) = discover_ip_and_gateway(&bv_config, ip, gateway, &nodes, id).await?;
        let vm_config = VmConfig::build_from(&image_variant);

        let node_info = bv_client
            .create_dev_node(NodeState {
                id,
                name: Petnames::default()
                    .generate_one(3, "-")
                    .ok_or(anyhow!("failed to generate node name"))?,
                protocol_id: "dev-node-protocol-id".to_string(),
                image_key: ProtocolImageKey {
                    protocol_key: image_variant.protocol_key,
                    variant_key: image_variant.variant_key,
                },
                dev_mode: true,
                ip,
                gateway,
                properties,
                firewall: image_variant.firewall_config.into(),
                display_name: "".to_string(),
                org_id: "dev-node-org-id".to_string(),
                org_name: "dev node org name".to_string(),
                protocol_name: "dev node protocol name".to_string(),
                dns_name: "dev.node.dns.name".to_string(),
                tags,
                vm_config,
                image: NodeImage {
                    id: "00000000-0000-0000-0000-000000000000".to_string(),
                    version: image_variant.version,
                    config_id: "00000000-0000-0000-0000-000000000000".to_string(),
                    archive_id: "00000000-0000-0000-0000-000000000000".to_string(),
                    store_key: "dev-node-store-id".to_string(),
                    uri: image_variant.container_uri,
                    min_babel_version: env!("CARGO_PKG_VERSION").to_string(),
                },
                assigned_cpus: vec![],
                expected_status: VmStatus::Stopped,
                started_at: None,
                initialized: false,
                restarting: false,
                upgrade_state: Default::default(),
                apptainer_config: None,
            })
            .await?
            .into_inner();
        Ok(Self {
            node_info,
            bv_client,
        })
    }

    async fn start(&mut self) -> eyre::Result<()> {
        self.bv_client.start_node(self.node_info.state.id).await?;
        Ok(())
    }

    async fn wait_for_running(&mut self, timeout: Duration) -> eyre::Result<()> {
        let start = std::time::Instant::now();
        while VmStatus::Running
            != self
                .bv_client
                .get_node(self.node_info.state.id)
                .await?
                .into_inner()
                .status
        {
            if start.elapsed() < timeout {
                sleep(Duration::from_secs(1)).await;
            } else {
                panic!("Node is not in Running state after {}s", timeout.as_secs())
            }
        }
        Ok(())
    }
    async fn run_checks(
        &mut self,
        start_timeout: u64,
        jobs_wait: u64,
        checks: Vec<NodeChecks>,
    ) -> eyre::Result<()> {
        self.start().await?;
        println!("Node started");
        self.wait_for_running(Duration::from_secs(start_timeout))
            .await?;
        println!("Node is running");
        sleep(Duration::from_secs(jobs_wait)).await;

        println!("Getting node metrics");
        let metrics = self
            .bv_client
            .get_node_metrics(self.node_info.state.id)
            .await?
            .into_inner();
        for (name, info) in metrics.jobs {
            if checks.contains(&NodeChecks::JobsStatus) {
                println!("Checking jobs status");
                if let babel_api::engine::JobStatus::Finished {
                    exit_code: Some(exit_code),
                    ..
                } = info.status
                {
                    ensure!(
                        exit_code == 0,
                        "job '{name}' finished with {exit_code} exit code"
                    );
                }
            }
            if checks.contains(&NodeChecks::JobsRestarts) {
                println!("Checking jobs restart_count");
                ensure!(
                    info.restart_count == 0,
                    "job '{name}' has been restarted {} times",
                    info.restart_count
                );
            }
        }
        if checks.contains(&NodeChecks::ProtocolStatus) {
            println!("Checking protocol status");
            let Some(protocol_status) = metrics.protocol_status else {
                bail!("No protocol status")
            };
            ensure!(
                protocol_status.health != babel_api::plugin::NodeHealth::Unhealthy,
                "Unhealthy protocol status"
            );
        }
        Ok(())
    }

    async fn delete(&mut self) -> eyre::Result<()> {
        self.bv_client.delete_node(self.node_info.state.id).await?;
        Ok(())
    }
}

impl From<nib_meta::Action> for firewall::Action {
    fn from(value: nib_meta::Action) -> Self {
        match value {
            nib_meta::Action::Allow => firewall::Action::Allow,
            nib_meta::Action::Deny => firewall::Action::Deny,
            nib_meta::Action::Reject => firewall::Action::Reject,
        }
    }
}

impl From<nib_meta::NetProtocol> for firewall::Protocol {
    fn from(value: nib_meta::NetProtocol) -> Self {
        match value {
            nib_meta::NetProtocol::Tcp => firewall::Protocol::Tcp,
            nib_meta::NetProtocol::Udp => firewall::Protocol::Udp,
            nib_meta::NetProtocol::Both => firewall::Protocol::Both,
        }
    }
}

impl From<nib_meta::Direction> for firewall::Direction {
    fn from(value: nib_meta::Direction) -> Self {
        match value {
            nib_meta::Direction::In => firewall::Direction::In,
            nib_meta::Direction::Out => firewall::Direction::Out,
        }
    }
}

impl From<nib_meta::FirewallRule> for firewall::Rule {
    fn from(value: nib_meta::FirewallRule) -> Self {
        Self {
            name: value.key,
            action: value.action.into(),
            direction: value.direction.into(),
            protocol: Some(value.protocol.into()),
            ips: value.ips.into_iter().map(|ip| ip.ip).collect(),
            ports: value.ports.into_iter().map(|port| port.port).collect(),
        }
    }
}

impl From<nib_meta::FirewallConfig> for firewall::Config {
    fn from(value: nib_meta::FirewallConfig) -> Self {
        Self {
            default_out: value.default_out.into(),
            default_in: value.default_in.into(),
            rules: value.rules.into_iter().map(|rule| rule.into()).collect(),
        }
    }
}

impl VmConfig {
    fn build_from(value: &ImageVariant) -> Self {
        Self {
            vcpu_count: value.min_cpu as usize,
            mem_size_mb: value.min_memory_mb,
            disk_size_gb: value.min_disk_gb,
            ramdisks: value
                .ramdisks
                .iter()
                .map(|ramdisk| RamdiskConfiguration {
                    ram_disk_mount_point: ramdisk.mount.clone(),
                    ram_disk_size_mb: ramdisk.size_mb,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use crate::nib::{pick_variant, ImageVariant};
    use crate::{nib_meta, utils};
    use assert_fs::TempDir;
    use std::fs;

    #[test]
    pub fn test_template() {
        let tmp_root = TempDir::new().unwrap().to_path_buf();
        fs::create_dir_all(&tmp_root).unwrap();
        let babel_path = tmp_root.join("template.yaml");
        let params = [
            ("protocol_key", "test_protocol"),
            ("variant_key", "test_variant"),
            ("client", "test_client"),
            ("network", "test_network"),
            ("node_type", "test_node_type"),
        ];
        utils::render_template(
            include_str!("../data/babel.yaml.template"),
            &babel_path,
            &params,
        )
        .unwrap();

        let image =
            serde_yaml_ng::from_str::<nib_meta::Image>(&fs::read_to_string(babel_path).unwrap())
                .unwrap();
        let image_variant =
            ImageVariant::build(&image, pick_variant(image.variants.clone(), None).unwrap());
        println!("{}", serde_json::to_string_pretty(&image_variant).unwrap());
    }
}
