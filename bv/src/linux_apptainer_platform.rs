use crate::node_env::NodeEnv;
use crate::{
    apptainer_machine,
    bv_context::BvContext,
    config,
    config::{ApptainerConfig, SharedConfig},
    linux_platform,
    node::NODE_REQUEST_TIMEOUT,
    node_context,
    node_state::NodeState,
    nodes_manager::NodesDataCache,
    pal::{self, AvailableResources, NodeConnection, Pal},
    services,
    utils::is_dev_ip,
};
use async_trait::async_trait;
use babel_api::metadata::firewall;
use bv_utils::cmd::run_cmd;
use bv_utils::with_retry;
use cidr_utils::cidr::Ipv4Cidr;
use eyre::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::str::FromStr;
use std::{
    net::IpAddr,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};
use tracing::debug;
use uuid::Uuid;

const ENGINE_SOCKET_NAME: &str = "engine.socket";
const BABEL_SOCKET_NAME: &str = "babel.socket";

#[derive(Debug)]
pub struct LinuxApptainerPlatform {
    base: linux_platform::LinuxPlatform,
    bridge_ip: IpAddr,
    mask_bits: u8,
    config: ApptainerConfig,
}

impl Deref for LinuxApptainerPlatform {
    type Target = linux_platform::LinuxPlatform;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for LinuxApptainerPlatform {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

impl LinuxApptainerPlatform {
    pub async fn default() -> Result<Self> {
        Ok(Self {
            base: linux_platform::LinuxPlatform::new().await?,
            bridge_ip: IpAddr::from_str("127.0.0.1")?,
            mask_bits: 28,
            config: ApptainerConfig {
                extra_args: None,
                host_network: false,
                cpu_limit: false,
                memory_limit: false,
            },
        })
    }

    pub async fn new(iface: &str, config: ApptainerConfig) -> Result<Self> {
        let routes = run_cmd("ip", ["--json", "route"]).await?;
        let mut routes: Vec<crate::utils::IpRoute> = serde_json::from_str(&routes)?;
        routes.retain(|route| is_dev_ip(route, iface));
        let route = routes
            .pop()
            .ok_or(anyhow!("can't find {iface} ip in routing table"))?;
        let cidr = Ipv4Cidr::from_str(&route.dst)
            .with_context(|| format!("cannot parse {} as cidr", route.dst))?;

        Ok(Self {
            base: linux_platform::LinuxPlatform::new().await?,
            bridge_ip: IpAddr::from_str(&route.prefsrc.unwrap())?, // can safely unwrap here
            mask_bits: cidr.get_bits(),
            config,
        })
    }
}

#[async_trait]
impl Pal for LinuxApptainerPlatform {
    fn bv_root(&self) -> &Path {
        self.base.bv_root.as_path()
    }

    fn babel_path(&self) -> &Path {
        self.base.babel_path.as_path()
    }

    fn job_runner_path(&self) -> &Path {
        self.base.job_runner_path.as_path()
    }

    type CommandsStream = services::mqtt::MqttStream;
    type CommandsStreamConnector = services::mqtt::MqttConnector;
    fn create_commands_stream_connector(
        &self,
        config: &SharedConfig,
    ) -> Self::CommandsStreamConnector {
        services::mqtt::MqttConnector {
            config: config.clone(),
        }
    }

    type ApiServiceConnector = services::DefaultConnector;
    fn create_api_service_connector(&self, config: &SharedConfig) -> Self::ApiServiceConnector {
        services::DefaultConnector {
            config: config.clone(),
        }
    }

    type NodeConnection = BareNodeConnection;
    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection {
        BareNodeConnection::new(node_context::build_node_dir(self.bv_root(), node_id))
    }

    type VirtualMachine = apptainer_machine::ApptainerMachine;

    async fn create_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<Self::VirtualMachine> {
        apptainer_machine::new(
            &self.bv_root,
            self.bridge_ip,
            self.mask_bits,
            NodeEnv::new(bv_context, node_state),
            node_state,
            self.babel_path.clone(),
            self.config.clone(),
        )
        .await?
        .create()
        .await
    }

    async fn attach_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<Self::VirtualMachine> {
        apptainer_machine::new(
            &self.bv_root,
            self.bridge_ip,
            self.mask_bits,
            NodeEnv::new(bv_context, node_state),
            node_state,
            self.babel_path.clone(),
            self.config.clone(),
        )
        .await?
        .attach()
        .await
    }

    fn available_resources(&self, nodes_data_cache: &NodesDataCache) -> Result<AvailableResources> {
        self.base.available_resources(
            nodes_data_cache,
            self.used_disk_space_correction(nodes_data_cache)?,
        )
    }

    fn used_disk_space_correction(&self, nodes_data_cache: &NodesDataCache) -> Result<u64> {
        let mut correction = 0;
        for (id, data) in nodes_data_cache {
            let data_img_path = apptainer_machine::build_rootfs_dir(&node_context::build_node_dir(
                self.bv_root(),
                *id,
            ))
            .join(babel_api::engine::DATA_DRIVE_MOUNT_POINT.trim_start_matches('/'));
            let actual_data_size = fs_extra::dir::get_size(&data_img_path)
                .with_context(|| format!("can't check size of '{}'", data_img_path.display()))?;
            let declared_data_size = data.requirements.disk_size_gb * 1_000_000_000;
            debug!("id: {id}; declared: {declared_data_size}; actual: {actual_data_size}");
            if declared_data_size > actual_data_size {
                correction += declared_data_size - actual_data_size;
            }
        }
        Ok(correction)
    }

    type RecoveryBackoff = linux_platform::RecoveryBackoff;
    fn create_recovery_backoff(&self) -> Self::RecoveryBackoff {
        Default::default()
    }

    async fn apply_firewall_config(
        &self,
        node_id: Uuid,
        node_ip: IpAddr,
        config: firewall::Config,
    ) -> Result<()> {
        self.base
            .apply_firewall_config(node_id, node_ip, config)
            .await
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct LinuxNetInterface {
    pub name: String,
    #[serde(default = "default_bridge_ifa")]
    pub bridge_ifa: String,
    pub ip: IpAddr,
    pub gateway: IpAddr,
}

fn default_bridge_ifa() -> String {
    config::DEFAULT_BRIDGE_IFACE.to_string()
}

#[derive(Debug)]
enum NodeConnectionState {
    Closed,
    Broken,
    Babel(pal::BabelClient),
}

#[derive(Debug)]
pub struct BareNodeConnection {
    babel_socket_path: PathBuf,
    engine_socket_path: PathBuf,
    state: NodeConnectionState,
}

impl BareNodeConnection {
    pub fn new(node_path: PathBuf) -> Self {
        let rootfs_path = apptainer_machine::build_rootfs_dir(&node_path);
        Self {
            babel_socket_path: rootfs_path.join(BABEL_SOCKET_NAME),
            engine_socket_path: rootfs_path.join(ENGINE_SOCKET_NAME),
            state: NodeConnectionState::Closed,
        }
    }
}

#[async_trait]
impl NodeConnection for BareNodeConnection {
    async fn setup(&mut self) -> Result<()> {
        self.attach().await
    }

    async fn attach(&mut self) -> Result<()> {
        self.state = NodeConnectionState::Babel(
            babel_api::babel::babel_client::BabelClient::with_interceptor(
                bv_utils::rpc::build_socket_channel(&self.babel_socket_path),
                bv_utils::rpc::DefaultTimeout(NODE_REQUEST_TIMEOUT),
            ),
        );
        Ok(())
    }

    fn close(&mut self) {
        self.state = NodeConnectionState::Closed;
    }

    fn is_closed(&self) -> bool {
        matches!(self.state, NodeConnectionState::Closed)
    }

    fn mark_broken(&mut self) {
        self.state = NodeConnectionState::Broken;
    }

    fn is_broken(&self) -> bool {
        matches!(self.state, NodeConnectionState::Broken)
    }

    async fn test(&mut self) -> Result<()> {
        let mut client = babel_api::babel::babel_client::BabelClient::with_interceptor(
            bv_utils::rpc::build_socket_channel(&self.babel_socket_path),
            bv_utils::rpc::DefaultTimeout(NODE_REQUEST_TIMEOUT),
        );
        with_retry!(client.get_version(()))?;
        // update connection state (otherwise it still may be seen as broken)
        self.state = NodeConnectionState::Babel(client);
        Ok(())
    }

    async fn babel_client(&mut self) -> Result<&mut pal::BabelClient> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("node connection is closed")
            }
            NodeConnectionState::Babel { .. } => {}
            NodeConnectionState::Broken => {
                debug!("Reconnecting to babel");
                self.attach().await?;
            }
        };
        if let NodeConnectionState::Babel(client) = &mut self.state {
            Ok(client)
        } else {
            unreachable!()
        }
    }

    fn engine_socket_path(&self) -> &Path {
        &self.engine_socket_path
    }
}
