use crate::{
    apptainer_machine::{PLUGIN_MAIN_FILENAME, PLUGIN_PATH},
    bib_cli::{ImageCommand, ProtocolCommand},
    bv_config, firewall,
    internal_server::{self, NodeDisplayInfo},
    node_state::{NodeImage, NodeState, ProtocolImageKey, VmConfig, VmStatus},
    protocol,
    services::{self, ApiServiceConnector},
    utils, workspace,
};
use babel_api::{
    engine::NodeEnv,
    rhai_plugin_linter,
    utils::{BabelConfig, RamdiskConfiguration},
};
use bv_utils::cmd::run_cmd;
use eyre::anyhow;
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

            let properties = if let Some(props) = props {
                serde_yaml::from_str(&props)?
            } else {
                Default::default()
            };
            let nodes = bv_client.get_nodes(()).await?.into_inner();
            let (ip, gateway) =
                discover_ip_and_gateway(&bv_config.iface, ip, gateway, &nodes).await?;

            let node = bv_client
                .create_dev_node(internal_server::CreateDevNodeRequest {
                    new_node_state: NodeState {
                        id: Uuid::new_v4(),
                        name: Petnames::default().generate_one(3, "-"),
                        protocol_id: "dev-node-protocol-id".to_string(),
                        image_key: ProtocolImageKey {
                            protocol_key: local_image.key.protocol_key,
                            variant_key: local_image.key.variant_key,
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
                        vm_config: VmConfig {
                            vcpu_count: 0,
                            mem_size_mb: 0,
                            disk_size_gb: 0,
                            babel_config: BabelConfig {
                                ramdisks: local_image
                                    .ramdisks
                                    .into_iter()
                                    .map(|ramdisk| RamdiskConfiguration {
                                        ram_disk_mount_point: ramdisk.mount,
                                        ram_disk_size_mb: ramdisk.size_bytes,
                                    })
                                    .collect(),
                            },
                        },
                        image: NodeImage {
                            id: "dev-node-image-id".to_string(),
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
                    },
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
        ImageCommand::Check { props, path } => {
            let local_image: protocol::Image =
                serde_yaml::from_str(&fs::read_to_string(&path).await?)?;
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
                        node_protocol: local_image.key.protocol_key,
                        node_variant: local_image.key.variant_key,
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
            let protocol_version_id =
                match client.get_protocol_version(local_image.key.clone()).await? {
                    Some(remote) if remote.semantic_version == local_image.version => {
                        client
                            .update_protocol_version(
                                remote.protocol_version_id.clone(),
                                local_image.clone(),
                            )
                            .await?;
                        remote.protocol_version_id
                    }
                    _ => {
                        client
                            .add_protocol_version(local_image.clone())
                            .await?
                            .protocol_version_id
                    }
                };
            if let Some(remote) = client
                .get_image(
                    local_image.key.clone(),
                    Some(local_image.version.clone()),
                    None,
                )
                .await?
            {
                client
                    .update_image(remote.image_id, local_image.clone())
                    .await?;
            } else {
                client
                    .add_image(protocol_version_id, local_image.clone())
                    .await?;
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
                if let Some(remote) = protocols.iter().find(|protocol| protocol.key == local.key) {
                    client
                        .update_protocol(remote.protocol_id.clone(), local)
                        .await?;
                } else {
                    client.add_protocol(local).await?;
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
