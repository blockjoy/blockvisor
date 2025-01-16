use crate::nib_meta::{ArchivePointer, FirewallConfig, ImageProperty, RamdiskConfig, Visibility};
use crate::{
    apptainer_machine::{PLUGIN_MAIN_FILENAME, PLUGIN_PATH},
    bv_config, firewall,
    internal_server::{self, NodeDisplayInfo},
    nib_cli::{ImageCommand, ProtocolCommand},
    nib_meta::{self, Variant},
    node_state::{NodeImage, NodeState, ProtocolImageKey, VmConfig, VmStatus},
    services::{self, protocol::PushResult, ApiServiceConnector},
    utils,
};
use babel_api::{engine::NodeEnv, rhai_plugin_linter, utils::RamdiskConfiguration};
use bv_utils::cmd::run_cmd;
use eyre::{anyhow, bail, ensure, Context};
use petname::Petnames;
use serde::Serialize;
use std::{
    ffi::OsStr,
    net,
    {
        collections::HashMap,
        path::{Path, PathBuf},
        str::FromStr,
        time::Duration,
    },
};
use tokio::fs;
use tonic::transport::Endpoint;
use tracing::info;
use uuid::Uuid;

const BV_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BV_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
            let params = [
                ("protocol_key", protocol.as_str()),
                ("variant_key", variant.as_str()),
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
            path,
            variant,
        } => {
            let bv_config = load_bv_config(bv_root).await?;
            let bv_url = format!("http://localhost:{}", bv_config.blockvisor_port);
            let mut bv_client = internal_server::service_client::ServiceClient::connect(
                Endpoint::from_shared(bv_url)?.connect_timeout(BV_CONNECT_TIMEOUT),
            )
            .await?;
            let image: nib_meta::Image = serde_yaml_ng::from_str(&fs::read_to_string(path).await?)?;
            let variant = pick_variant(image.variants.clone(), variant)?;
            let vm_config = VmConfig::build_from(&variant);
            let image_variant = ImageVariant::build(&image, variant);
            let properties = build_properties(&image_variant.properties, props)?;
            let nodes = bv_client.get_nodes(()).await?.into_inner();
            let id = Uuid::new_v4();
            let (ip, gateway) =
                discover_ip_and_gateway(&bv_config, ip, gateway, &nodes, id).await?;

            let node = bv_client
                .create_dev_node(NodeState {
                    id,
                    name: Petnames::default().generate_one(3, "-"),
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
            println!(
                "Created new dev_node with ID `{}` and name `{}`\n{:#?}",
                node.state.id, node.state.name, node.state
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
            node.vm_config = VmConfig::build_from(variant);
            let image_variant = ImageVariant::build(&image, variant.clone());
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
            props,
            path,
            variant,
        } => {
            let image: nib_meta::Image =
                serde_yaml_ng::from_str(&fs::read_to_string(&path).await?)?;
            let variant = pick_variant(image.variants.clone(), variant)?;
            let image_variant = ImageVariant::build(&image, variant.clone());
            let properties = build_properties(&image_variant.properties, props)?;
            let tmp_dir = tempdir::TempDir::new("nib_check")?;
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
            let res = rhai_plugin_linter::check(
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
                    bv_host_name: "nostname".to_string(),
                    bv_api_url: "none.com".to_string(),
                    node_org_id: "org-id".to_string(),
                    data_mount_point: PathBuf::from("/blockjoy"),
                    protocol_data_path: PathBuf::from("/blockjoy/protocol_data"),
                },
                properties,
            );
            println!("Plugin linter: {res:?}");
            res?;
        }
        ImageCommand::Push {
            min_babel_version,
            path,
        } => {
            let min_babel_version =
                min_babel_version.unwrap_or(env!("CARGO_PKG_VERSION").to_string());
            let mut client = services::protocol::ProtocolService::new(connector).await?;
            let image: nib_meta::Image = serde_yaml_ng::from_str(&fs::read_to_string(path).await?)?;
            for variant in &image.variants {
                let image_variant = ImageVariant::build(&image, variant.clone());
                let image_key = ProtocolImageKey {
                    protocol_key: image_variant.protocol_key.clone(),
                    variant_key: image_variant.variant_key.clone(),
                };

                ensure!(
                    image_variant
                        .sku_code
                        .chars()
                        .all(|character| character == '-'
                            || character.is_ascii_digit()
                            || character.is_ascii_uppercase())
                        && image_variant.sku_code.split("-").count() == 3,
                    "invalud SKU format for variant '{}'",
                    image_variant.variant_key
                );

                let protocol_version_id =
                    match client.get_protocol_version(image_key.clone()).await? {
                        Some(remote) if remote.semantic_version == image_variant.version => {
                            client
                                .update_protocol_version(remote.clone(), image_variant.clone())
                                .await?;
                            println!(
                                "Protocol version '{}/{}/{}' updated",
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
                                "Protocol version '{}/{}/{}' added",
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
}

impl ImageVariant {
    fn build(image: &nib_meta::Image, variant: Variant) -> Self {
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
        }
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
    fn build_from(value: &Variant) -> Self {
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
