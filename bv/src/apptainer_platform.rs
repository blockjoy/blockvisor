use crate::{
    apptainer_machine, bv_config,
    bv_config::{ApptainerConfig, SharedConfig},
    bv_context::BvContext,
    linux_platform,
    node::NODE_REQUEST_TIMEOUT,
    node_context,
    node_state::NodeState,
    nodes_manager::NodesDataCache,
    pal::{self, AvailableResources, NodeConnection, NodeFirewallConfig, Pal, VirtualMachine},
    services, ufw_wrapper,
};
use async_trait::async_trait;
use bv_utils::with_retry;
use eyre::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Debug,
    net::IpAddr,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};
use tracing::{debug, error};
use uuid::Uuid;

const ENGINE_SOCKET_NAME: &str = "engine.socket";
const BABEL_SOCKET_NAME: &str = "babel.socket";

#[derive(Debug)]
pub struct ApptainerPlatform {
    base: linux_platform::LinuxPlatform,
    bridge_ip: IpAddr,
    prefix: u8,
    config: ApptainerConfig,
}

impl Deref for ApptainerPlatform {
    type Target = linux_platform::LinuxPlatform;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for ApptainerPlatform {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

impl ApptainerPlatform {
    pub async fn default() -> Result<Self> {
        Ok(Self {
            base: linux_platform::LinuxPlatform::new().await?,
            bridge_ip: IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            prefix: 0,
            config: Default::default(),
        })
    }

    pub async fn new(config: &bv_config::Config) -> Result<Self> {
        Ok(Self {
            base: linux_platform::LinuxPlatform::new().await?,
            bridge_ip: config.net_conf.host_ip,
            prefix: config.net_conf.prefix,
            config: config.apptainer.clone(),
        })
    }

    async fn new_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<apptainer_machine::ApptainerMachine> {
        // LEGACY node support - remove once all nodes upgraded
        if node_state.image.uri.starts_with("legacy://") {
            apptainer_machine::new_legacy(
                &self.bv_root,
                self.bridge_ip,
                self.prefix,
                bv_context,
                node_state,
                self.babel_path.clone(),
                node_state
                    .apptainer_config
                    .clone()
                    .unwrap_or(self.config.clone()),
            )
            .await
            .inspect_err(|err| error!("legacy node load failed: {err:#}"))
        } else {
            apptainer_machine::new(
                &self.bv_root,
                self.bridge_ip,
                self.prefix,
                bv_context,
                node_state,
                self.babel_path.clone(),
                node_state
                    .apptainer_config
                    .clone()
                    .unwrap_or(self.config.clone()),
            )
            .await
        }
    }
}

#[async_trait]
impl Pal for ApptainerPlatform {
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
        let mut vm = self.new_vm(bv_context, node_state).await?;
        if let Err(err) = vm.build().await {
            vm.delete().await?;
            Err(err)
        } else {
            Ok(vm)
        }
    }

    async fn attach_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<Self::VirtualMachine> {
        let mut vm = self.new_vm(bv_context, node_state).await?;
        vm.attach().await?;
        Ok(vm)
    }

    async fn available_cpus(&self) -> usize {
        linux_platform::available_cpus()
    }

    async fn available_resources(
        &self,
        nodes_data_cache: NodesDataCache,
    ) -> Result<AvailableResources> {
        self.base
            .available_resources(
                nodes_data_cache.clone(),
                self.used_disk_space_correction(nodes_data_cache).await?,
            )
            .await
    }

    async fn used_disk_space_correction(&self, nodes_data_cache: NodesDataCache) -> Result<u64> {
        let bv_root = self.bv_root().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut correction = 0;
            for (id, data) in nodes_data_cache {
                let data_img_path =
                    node_context::build_node_dir(&bv_root, id).join(apptainer_machine::DATA_DIR);
                let actual_data_size =
                    fs_extra::dir::get_size(&data_img_path).with_context(|| {
                        format!("can't check size of '{}'", data_img_path.display())
                    })?;
                let declared_data_size = data.vm_config.disk_size_gb * 1_000_000_000;
                debug!("id: {id}; declared: {declared_data_size}; actual: {actual_data_size}");
                if declared_data_size > actual_data_size {
                    correction += declared_data_size - actual_data_size;
                }
            }
            Ok(correction)
        })
        .await?
    }

    type RecoveryBackoff = linux_platform::RecoveryBackoff;
    fn create_recovery_backoff(&self) -> Self::RecoveryBackoff {
        Default::default()
    }

    async fn apply_firewall_config(&self, config: NodeFirewallConfig) -> Result<()> {
        ufw_wrapper::apply_firewall_config(config).await
    }

    async fn cleanup_firewall_config(&self, id: Uuid) -> Result<()> {
        ufw_wrapper::cleanup_node_rules(id).await
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
    bv_config::DEFAULT_BRIDGE_IFACE.to_string()
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
