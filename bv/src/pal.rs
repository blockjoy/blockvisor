/// Platform Abstraction Layer is a helper module which goal is to increase testability of BV.
/// Original intention is testability, not portability, nevertheless it may be useful if such requirement appear.
///
/// It defines `Pal` trait which is top level abstraction that contains definitions of sub layers.
///
use crate::{config::SharedConfig, node_data::NodeData, nodes_manager::NodesDataCache, services};
use async_trait::async_trait;
use babel_api::metadata::Requirements;
use eyre::Result;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fmt::Debug,
    net::IpAddr,
    path::{Path, PathBuf},
};
use sysinfo::Pid;
use tonic::{codegen::InterceptedService, transport::Channel};
use uuid::Uuid;

/// Platform Abstraction Layer - trait used to detach business logic form platform specifics, so it
/// can be easily tested.
#[async_trait]
pub trait Pal {
    /// Root directory for all BV paths. Instead, abstracting whole file system, just let tests
    /// work in their own space - kind of 'chroot'.
    fn bv_root(&self) -> &Path;

    /// Path to babel binary bundled with this BV.
    fn babel_path(&self) -> &Path;

    /// Path to job runner binary bundled with this BV.
    fn job_runner_path(&self) -> &Path;

    /// Type representing network interface. It is required to be Serialize/Deserialize
    /// since it's going to be part of node data.
    type NetInterface: NetInterface + Serialize + DeserializeOwned + Debug + Clone;
    /// Creates the new network interface and add it to our bridge.
    /// The `ip` is not assigned on the host but rather by the API.
    async fn create_net_interface(
        &self,
        index: u32,
        ip: IpAddr,
        gateway: IpAddr,
        config: &SharedConfig,
    ) -> Result<Self::NetInterface>;

    /// Type representing commands stream.
    type CommandsStream: CommandsStream;
    /// Type representing commands stream connector.
    type CommandsStreamConnector: ServiceConnector<Self::CommandsStream>;
    /// Creates commands stream connector.
    fn create_commands_stream_connector(
        &self,
        config: &SharedConfig,
    ) -> Self::CommandsStreamConnector;

    /// Type representing API service connector.
    type ApiServiceConnector: services::ApiServiceConnector + Clone;
    /// Creates commands stream connector.
    fn create_api_service_connector(&self, config: &SharedConfig) -> Self::ApiServiceConnector;

    /// Type representing node connection.
    type NodeConnection: NodeConnection + Debug;
    /// Created node connection, so it can be used to communicate with Babel and BabelSup.
    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection;

    /// Type representing virtual machine on which node is running.
    type VirtualMachine: VirtualMachine + Debug;
    /// Created new VM instance.
    async fn create_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine>;
    /// Attach to already created VM instance.
    async fn attach_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine>;
    /// Get all VMs associated process ids.
    fn get_vm_pids(&self) -> Result<Vec<Pid>>;
    /// Get process id associated with given VM.
    fn get_vm_pid(&self, vm_id: Uuid) -> Result<Pid>;

    /// Build path to VM data directory, a place where kernel and other VM related data are stored.
    fn build_vm_data_path(&self, id: Uuid) -> PathBuf;
    /// Get available resources, but take into account requirements declared by nodes.
    fn available_resources(&self, nodes_data_cache: &NodesDataCache) -> Result<AvailableResources>;
    /// Calculate used disk space value correction. Regarding sparse files used for data images, used
    /// disk space need manual correction that include declared data image size.
    fn used_disk_space_correction(&self, nodes_data_cache: &NodesDataCache) -> Result<u64>;

    /// Type representing recovery backoff counter.
    type RecoveryBackoff: RecoverBackoff + Debug;
    /// Created new VM instance.
    fn create_recovery_backoff(&self) -> Self::RecoveryBackoff;
}

pub type AvailableResources = Requirements;

#[async_trait]
pub trait NetInterface {
    fn name(&self) -> &String;
    fn ip(&self) -> &IpAddr;
    fn gateway(&self) -> &IpAddr;

    /// Remaster the network interface.
    async fn remaster(&self) -> Result<()>;
    /// Delete the network interface.
    async fn delete(&self) -> Result<()>;
}

#[async_trait]
pub trait ServiceConnector<S> {
    async fn connect(&self) -> Result<S>;
}

#[async_trait]
pub trait CommandsStream {
    /// Wait for next command. Returns pb::Command serialized with protobufs to bytes.
    async fn wait_for_pending_commands(&mut self) -> Result<Option<Vec<u8>>>;
}

pub type BabelClient = babel_api::babel::babel_client::BabelClient<
    InterceptedService<Channel, bv_utils::rpc::DefaultTimeout>,
>;
pub type BabelSupClient = babel_api::babelsup::babel_sup_client::BabelSupClient<
    InterceptedService<Channel, bv_utils::rpc::DefaultTimeout>,
>;

#[async_trait]
pub trait NodeConnection {
    /// Setup connection to just started node.
    async fn setup(&mut self) -> Result<()>;
    /// Attach to already running node.
    async fn attach(&mut self) -> Result<()>;
    /// Close opened connection.
    fn close(&mut self);
    /// Check if connection is closed.
    fn is_closed(&self) -> bool;
    /// Mark connection as broken. It should be called whenever client detect some connectivity issues.
    /// Once connection is marked as broken, it will try to reestablish connection on next `*_client` call.
    fn mark_broken(&mut self);
    /// Check if connection was marked as broken.
    fn is_broken(&self) -> bool;
    /// Perform basic connectivity test, to check actual connection state.
    /// It may mutate internal state it connection was marked as broken, but now test pass.
    async fn test(&mut self) -> Result<()>;
    /// Get reference to BabelSup rpc client. Try to reestablish connection if it's necessary.
    async fn babel_client(&mut self) -> Result<&mut BabelClient>;
    /// Path to UDS where BabelEngine should listen for messages form Babel.
    fn engine_socket_path(&self) -> &Path;
}

#[derive(Debug, PartialEq, Clone)]
pub enum VmState {
    /// Machine is not started or already shut down
    SHUTOFF,
    /// Machine is running
    RUNNING,
}

#[async_trait]
pub trait VirtualMachine {
    /// Checks the VM actual state
    fn state(&self) -> VmState;
    /// Deletes the VM, cleaning up all associated resources.
    async fn delete(&mut self) -> Result<()>;
    /// Request for graceful shutdown of the VM.
    async fn shutdown(&mut self) -> Result<()>;
    /// Forcefully shutdown the VM.
    async fn force_shutdown(&mut self) -> Result<()>;
    /// Start the VM.
    async fn start(&mut self) -> Result<()>;
}

pub trait RecoverBackoff {
    fn backoff(&self) -> bool;
    fn reset(&mut self);
    fn start_failed(&mut self) -> bool;
    fn stop_failed(&mut self) -> bool;
    fn reconnect_failed(&mut self) -> bool;
}
