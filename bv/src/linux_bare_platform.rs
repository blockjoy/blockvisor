use crate::bare_machine::CHROOT_DIR;
/// Default Platform Abstraction Layer implementation for Linux.
use crate::{
    bare_machine,
    bare_machine::BABEL_BIN_NAME,
    config,
    config::SharedConfig,
    linux_platform,
    node_connection::RPC_REQUEST_TIMEOUT,
    node_data::NodeData,
    nodes_manager::NodesDataCache,
    pal,
    pal::{AvailableResources, NetInterface, NodeConnection, Pal},
    services, utils,
};
use async_trait::async_trait;
use bv_utils::with_retry;
use core::fmt;
use eyre::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::{
    net::IpAddr,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};
use sysinfo::Pid;
use tracing::debug;
use uuid::Uuid;

const ENGINE_SOCKET_NAME: &str = "engine.socket";
const BABEL_SOCKET_NAME: &str = "babel.socket";

#[derive(Debug)]
pub struct LinuxBarePlatform(linux_platform::LinuxPlatform);

impl Deref for LinuxBarePlatform {
    type Target = linux_platform::LinuxPlatform;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for LinuxBarePlatform {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl LinuxBarePlatform {
    pub async fn new() -> Result<Self> {
        Ok(Self(linux_platform::LinuxPlatform::new().await?))
    }
}

#[async_trait]
impl Pal for LinuxBarePlatform {
    fn bv_root(&self) -> &Path {
        self.0.bv_root.as_path()
    }

    fn babel_path(&self) -> &Path {
        self.0.babel_path.as_path()
    }

    fn job_runner_path(&self) -> &Path {
        self.0.job_runner_path.as_path()
    }

    type NetInterface = LinuxNetInterface;

    async fn create_net_interface(
        &self,
        index: u32,
        ip: IpAddr,
        gateway: IpAddr,
        config: &SharedConfig,
    ) -> Result<Self::NetInterface> {
        let name = format!("bv{index}");
        Ok(LinuxNetInterface {
            name,
            bridge_ifa: config.read().await.iface.clone(),
            ip,
            gateway,
        })
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
        BareNodeConnection::new(bare_machine::build_vm_data_path(self.bv_root(), node_id))
    }

    type VirtualMachine = bare_machine::BareMachine;

    async fn create_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        bare_machine::new(&self.bv_root, node_data, self.babel_path.clone())
            .await?
            .create()
            .await
    }

    async fn attach_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        bare_machine::new(&self.bv_root, node_data, self.babel_path.clone())
            .await?
            .attach()
            .await
    }

    fn get_vm_pids(&self) -> Result<Vec<Pid>> {
        utils::get_all_processes_pids(BABEL_BIN_NAME)
    }

    fn get_vm_pid(&self, vm_id: Uuid) -> Result<Pid> {
        Ok(utils::get_process_pid(BABEL_BIN_NAME, &vm_id.to_string())?)
    }

    fn build_vm_data_path(&self, id: Uuid) -> PathBuf {
        bare_machine::build_vm_data_path(self.bv_root(), id)
    }

    fn available_resources(&self, nodes_data_cache: &NodesDataCache) -> Result<AvailableResources> {
        self.0.available_resources(
            nodes_data_cache,
            self.used_disk_space_correction(nodes_data_cache)?,
        )
    }

    fn used_disk_space_correction(&self, nodes_data_cache: &NodesDataCache) -> Result<u64> {
        let mut correction = 0;
        for (id, data) in nodes_data_cache {
            let data_img_path = self
                .build_vm_data_path(*id)
                .join(bare_machine::CHROOT_DIR)
                .join(babel_api::engine::DATA_DRIVE_MOUNT_POINT);
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

#[async_trait]
impl NetInterface for LinuxNetInterface {
    fn name(&self) -> &String {
        &self.name
    }

    fn ip(&self) -> &IpAddr {
        &self.ip
    }

    fn gateway(&self) -> &IpAddr {
        &self.gateway
    }

    /// Remaster the network interface.
    async fn remaster(&self) -> Result<()> {
        Ok(())
    }

    /// Delete the network interface.
    async fn delete(&self) -> Result<()> {
        Ok(())
    }
}

impl fmt::Display for LinuxNetInterface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ip)
    }
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
    fn new(vm_path: PathBuf) -> Self {
        Self {
            babel_socket_path: vm_path.join(CHROOT_DIR).join(BABEL_SOCKET_NAME),
            engine_socket_path: vm_path.join(CHROOT_DIR).join(ENGINE_SOCKET_NAME),
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
                bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
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
            bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
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
