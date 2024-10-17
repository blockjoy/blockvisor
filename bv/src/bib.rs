use crate::{
    apptainer_machine::{PLUGIN_MAIN_FILENAME, PLUGIN_PATH},
    bib_cli::{ImageCommand, ProtocolCommand},
    bv_config, firewall,
    internal_server::{self, NodeDisplayInfo},
    node_state::{NodeImage, NodeState, ProtocolImageKey, VmConfig, VmStatus},
    protocol::{self, Variant},
    services::{self, protocol::PushResult, ApiServiceConnector},
    utils::{self, node_id_with_fallback},
    workspace,
};
use babel_api::{
    engine::NodeEnv,
    rhai_plugin_linter,
    utils::{BabelConfig, RamdiskConfiguration},
};
use bv_utils::cmd::run_cmd;
use eyre::{anyhow, bail};
use petname::Petnames;
use std::{
    ffi::OsStr,
    {net::IpAddr, path::Path, str::FromStr, time::Duration},
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
        ImageCommand::Create { protocol, variant } => {
            let params = [
                ("protocol_key", protocol.as_str()),
                ("variant_key", variant.as_str()),
                ("babel_version", env!("CARGO_PKG_VERSION")),
            ];
            let image_path = std::env::current_dir()?.join(&variant);
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
            let local_image: protocol::Image =
                serde_yaml::from_str(&fs::read_to_string(path).await?)?;
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
                Endpoint::from_shared(bv_url)?
                    .connect_timeout(BV_CONNECT_TIMEOUT)
                    .timeout(BV_REQUEST_TIMEOUT),
            )
            .await?;
            let local_image: protocol::Image =
                serde_yaml::from_str(&fs::read_to_string(path).await?)?;

            let variant = pick_variant(local_image.variants, variant)?;

            let properties = if let Some(props) = props {
                serde_yaml::from_str(&props)?
            } else {
                Default::default()
            };
            let nodes = bv_client.get_nodes(()).await?.into_inner();
            let (ip, gateway) =
                discover_ip_and_gateway(&bv_config.iface, ip, gateway, &nodes).await?;

            let node = bv_client
                .create_dev_node(NodeState {
                    id: Uuid::new_v4(),
                    name: Petnames::default().generate_one(3, "-"),
                    protocol_id: "dev-node-protocol-id".to_string(),
                    image_key: ProtocolImageKey {
                        protocol_key: local_image.protocol_key,
                        variant_key: variant.key.clone(),
                    },
                    dev_mode: true,
                    ip,
                    gateway,
                    properties,
                    firewall: local_image.firewall_config.into(),
                    display_name: "".to_string(),
                    org_id: "dev-node-org-id".to_string(),
                    org_name: "dev node org name".to_string(),
                    protocol_name: "dev node protocol name".to_string(),
                    dns_name: "dev.node.dns.name".to_string(),
                    vm_config: variant.into(),
                    image: NodeImage {
                        id: format!("dev-node-image-id-{}", local_image.version),
                        version: local_image.version,
                        config_id: "dev-node-config-id".to_string(),
                        archive_id: "dev-node-archive-id".to_string(),
                        uri: local_image.container_uri,
                    },
                    assigned_cpus: vec![],
                    expected_status: VmStatus::Stopped,
                    started_at: None,
                    initialized: false,
                    restarting: false,
                    apptainer_config: None,
                })
                .await?
                .into_inner();
            println!(
                "Created new dev_node with ID `{}` and name `{}`\n{:#?}",
                node.state.id, node.state.name, node.state
            );
            let _ = workspace::set_active_node(
                &std::env::current_dir()?,
                node.state.id,
                &node.state.name,
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
            let id_or_name = node_id_with_fallback(id_or_name)?;
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
            let local_image: protocol::Image =
                serde_yaml::from_str(&fs::read_to_string(path).await?)?;
            let mut node = bv_client.get_node(id).await?.into_inner().state;
            node.firewall = local_image.firewall_config.into();
            let variant = local_image
                .variants
                .into_iter()
                .find(|variant| variant.key == node.image_key.variant_key)
                .ok_or(anyhow!(
                    "variant {} not found in image definition",
                    node.image_key.variant_key
                ))?;
            node.vm_config = variant.into();
            node.image.id = format!("dev-node-image-id-{}", local_image.version);
            node.image.version = local_image.version;
            node.image.uri = local_image.container_uri;
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
            let local_image: protocol::Image =
                serde_yaml::from_str(&fs::read_to_string(&path).await?)?;
            let variant = pick_variant(local_image.variants, variant)?;
            let properties = if let Some(props) = props {
                serde_yaml::from_str(&props)?
            } else {
                Default::default()
            };
            let tmp_dir = tempdir::TempDir::new("bib_check")?;
            let rootfs_path = tmp_dir.path();
            run_cmd(
                "apptainer",
                [
                    OsStr::new("build"),
                    OsStr::new("--sandbox"),
                    OsStr::new("--force"),
                    rootfs_path.as_os_str(),
                    OsStr::new(&local_image.container_uri),
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
            println!(
                "Plugin linter: {:?}",
                rhai_plugin_linter::check(
                    rootfs_path.join(PLUGIN_PATH).join(PLUGIN_MAIN_FILENAME),
                    NodeEnv {
                        node_id: "node-id".to_string(),
                        node_name: "node_name".to_string(),
                        node_version: local_image.version,
                        node_protocol: local_image.protocol_key,
                        node_variant: variant.key,
                        node_ip: "1.2.3.4".to_string(),
                        node_gateway: "4.3.2.1".to_string(),
                        dev_mode: true,
                        bv_host_id: "host-id".to_string(),
                        bv_host_name: "nostname".to_string(),
                        bv_api_url: "none.com".to_string(),
                        org_id: "org-id".to_string(),
                    },
                    properties,
                )
            );
        }
        ImageCommand::Push { path } => {
            let mut client = services::protocol::ProtocolService::new(connector).await?;
            let local_image: protocol::Image =
                serde_yaml::from_str(&fs::read_to_string(path).await?)?;
            for variant in &local_image.variants {
                let image_key = ProtocolImageKey {
                    protocol_key: local_image.protocol_key.clone(),
                    variant_key: variant.key.clone(),
                };
                let protocol_version_id =
                    match client.get_protocol_version(image_key.clone()).await? {
                        Some(remote) if remote.semantic_version == local_image.version => {
                            client
                                .update_protocol_version(remote.clone(), local_image.clone())
                                .await?;
                            println!(
                                "Protocol version '{}/{}/{}' updated",
                                local_image.protocol_key, variant.key, local_image.version
                            );
                            remote.protocol_version_id
                        }
                        _ => {
                            let protocol_version_id = client
                                .add_protocol_version(local_image.clone(), variant.clone())
                                .await?
                                .protocol_version_id;
                            println!(
                                "Protocol version '{}/{}/{}' added",
                                local_image.protocol_key, variant.key, local_image.version
                            );
                            protocol_version_id
                        }
                    };
                match client
                    .push_image(protocol_version_id, local_image.clone(), variant.clone())
                    .await?
                {
                    PushResult::Added(image) => println!(
                        "Image '{}/{}/{}/{}' added",
                        local_image.protocol_key,
                        variant.key,
                        local_image.version,
                        image.build_version,
                    ),
                    PushResult::Updated(image) => println!(
                        "Image '{}/{}/{}/{}' updated",
                        local_image.protocol_key,
                        variant.key,
                        local_image.version,
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
    let protocols = client.list_protocols().await?;
    match command {
        ProtocolCommand::List => {
            for protocol in protocols {
                println!(
                    "{} ({}) - {}",
                    protocol.name,
                    protocol.key,
                    protocol.description.unwrap_or_default()
                );
            }
        }
        ProtocolCommand::Push { path } => {
            let local_protocols: Vec<protocol::Protocol> =
                serde_yaml::from_str(&fs::read_to_string(path).await?)?;
            for local in local_protocols {
                let protocol_key = local.key.clone();
                if let Some(remote) = protocols.iter().find(|protocol| protocol.key == local.key) {
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
    iface: &str,
    ip: Option<String>,
    gateway: Option<String>,
    nodes: &[NodeDisplayInfo],
) -> eyre::Result<(IpAddr, IpAddr)> {
    let net = utils::discover_net_params(iface).await.unwrap_or_default();
    let gateway = match gateway {
        None => {
            let gateway = net
                .gateway
                .clone()
                .ok_or(anyhow!("can't auto discover gateway - provide it manually",))?;
            info!("Auto-discovered gateway `{gateway}");
            gateway
        }
        Some(gateway) => gateway,
    };
    let ip = match ip {
        None => {
            let mut used_ips = vec![];
            used_ips.push(gateway.clone());
            if let Some(host_ip) = &net.ip {
                used_ips.push(host_ip.clone());
            }
            for node in nodes {
                used_ips.push(node.state.ip.to_string());
            }
            let ip = utils::next_available_ip(&net, &used_ips).map_err(|err| {
                anyhow!("failed to auto assign ip - provide it manually : {err:#}")
            })?;
            info!("Auto-assigned ip `{ip}`");
            ip
        }
        Some(ip) => ip,
    };
    Ok((IpAddr::from_str(&ip)?, IpAddr::from_str(&gateway)?))
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

impl From<protocol::Action> for firewall::Action {
    fn from(value: protocol::Action) -> Self {
        match value {
            protocol::Action::Allow => firewall::Action::Allow,
            protocol::Action::Deny => firewall::Action::Deny,
            protocol::Action::Reject => firewall::Action::Reject,
        }
    }
}

impl From<protocol::NetProtocol> for firewall::Protocol {
    fn from(value: protocol::NetProtocol) -> Self {
        match value {
            protocol::NetProtocol::Tcp => firewall::Protocol::Tcp,
            protocol::NetProtocol::Udp => firewall::Protocol::Udp,
            protocol::NetProtocol::Both => firewall::Protocol::Both,
        }
    }
}

impl From<protocol::Direction> for firewall::Direction {
    fn from(value: protocol::Direction) -> Self {
        match value {
            protocol::Direction::In => firewall::Direction::In,
            protocol::Direction::Out => firewall::Direction::Out,
        }
    }
}

impl From<protocol::FirewallRule> for firewall::Rule {
    fn from(value: protocol::FirewallRule) -> Self {
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

impl From<protocol::FirewallConfig> for firewall::Config {
    fn from(value: protocol::FirewallConfig) -> Self {
        Self {
            default_out: value.default_out.into(),
            default_in: value.default_in.into(),
            rules: value.rules.into_iter().map(|rule| rule.into()).collect(),
        }
    }
}

impl From<Variant> for VmConfig {
    fn from(value: Variant) -> Self {
        Self {
            vcpu_count: value.min_cpu as usize,
            mem_size_mb: value.min_memory_bytes / 1_000_000,
            disk_size_gb: value.min_disk_bytes / 1_000_000_000,
            babel_config: BabelConfig {
                ramdisks: value
                    .ramdisks
                    .into_iter()
                    .map(|ramdisk| RamdiskConfiguration {
                        ram_disk_mount_point: ramdisk.mount,
                        ram_disk_size_mb: ramdisk.size_bytes,
                    })
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
pub mod tests {
    use crate::{protocol, utils};
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
            ("babel_version", env!("CARGO_PKG_VERSION")),
        ];
        utils::render_template(
            include_str!("../data/babel.yaml.template"),
            &babel_path,
            &params,
        )
        .unwrap();

        let image =
            serde_yaml::from_str::<protocol::Image>(&fs::read_to_string(babel_path).unwrap())
                .unwrap();
        println!("{image:?}");
    }
}
