/// Platform Abstraction Layer is a helper module which goal is to increase testability of BV.
/// Original intention is testability, not portability, nevertheless it may be useful if such requirement appear.
///
/// It defines `Pal` trait which is top level abstraction that contains definitions of sub layers.
///
use crate::{
    bv_config::SharedConfig, bv_context::BvContext, firewall, node_state::NodeState,
    nodes_manager::NodesDataCache, services,
};
use async_trait::async_trait;
use babel_api::engine::NodeEnv;
use eyre::Result;
use std::path::PathBuf;
use std::{fmt::Debug, net::IpAddr, path::Path};
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
    /// Created node connection, so it can be used to communicate with Babel.
    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection;

    /// Type representing virtual machine on which node is running.
    type VirtualMachine: VirtualMachine + Debug;
    /// Created new VM instance.
    async fn create_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<Self::VirtualMachine>;
    /// Attach to already created VM instance.
    async fn attach_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<Self::VirtualMachine>;

    /// Get available cpus.
    async fn available_cpus(&self) -> usize;
    /// Get available resources, but take into account requirements declared by nodes.
    async fn available_resources(
        &self,
        nodes_data_cache: NodesDataCache,
    ) -> Result<AvailableResources>;
    /// Calculate used disk space value correction. Regarding sparse files used for data images, used
    /// disk space need manual correction that include declared data image size.
    async fn used_disk_space_correction(&self, nodes_data_cache: NodesDataCache) -> Result<u64>;

    /// Type representing recovery backoff counter.
    type RecoveryBackoff: RecoverBackoff + Debug;
    /// Created new VM instance.
    fn create_recovery_backoff(&self) -> Self::RecoveryBackoff;

    /// Apply node specific firewall rules.
    async fn apply_firewall_config(&self, config: NodeFirewallConfig) -> Result<()>;
    /// Cleanup node specific firewall rules.
    async fn cleanup_firewall_config(&self, id: Uuid) -> Result<()>;
}

#[derive(Debug, PartialEq)]
pub struct NodeFirewallConfig {
    pub id: Uuid,
    pub ip: IpAddr,
    pub bridge: Option<String>,
    pub config: firewall::Config,
}

#[derive(Debug)]
pub struct AvailableResources {
    /// Virtual cores to share with VM.
    pub vcpu_count: usize,
    /// RAM allocated to VM in MB.
    pub mem_size_mb: u64,
    /// Size of data drive for storing protocol data (not to be confused with OS drive).
    pub disk_size_gb: u64,
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
    /// Get reference to Babel rpc client. Try to reestablish connection if it's necessary.
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
    /// Machine is in invalid state - not stopped, but not fully functioning.
    INVALID,
}

#[async_trait]
pub trait VirtualMachine {
    /// Checks the VM actual state
    async fn state(&self) -> VmState;
    /// Deletes the VM, cleaning up all associated resources.
    async fn delete(&mut self) -> Result<()>;
    /// Request for graceful shutdown of the VM.
    async fn shutdown(&mut self) -> Result<()>;
    /// Forcefully shutdown the VM.
    async fn force_shutdown(&mut self) -> Result<()>;
    /// Start the VM.
    async fn start(&mut self) -> Result<()>;
    /// Upgrade VM according to expected node_state.
    async fn upgrade(&mut self, node_state: &NodeState) -> Result<()>;
    /// Drop VM backup saved during last upgrade.
    async fn drop_backup(&mut self) -> Result<()>;
    /// Roll back to old version saved during last upgrade.
    async fn rollback(&mut self) -> Result<()>;
    /// Try recover VM that is in INVALID state.
    async fn recover(&mut self) -> Result<()>;
    /// Get associated NodeEnv.
    fn node_env(&self) -> NodeEnv;
    /// Update associated NodeEnv with new NodeState.
    fn update_node_env(&mut self, node_state: &NodeState);
    /// Get plugin path.
    fn plugin_path(&self) -> PathBuf;
}

pub trait RecoverBackoff {
    fn backoff(&self) -> bool;
    fn reset(&mut self);
    fn start_failed(&mut self) -> bool;
    fn stop_failed(&mut self) -> bool;
    fn reconnect_failed(&mut self) -> bool;
    fn vm_recovery_failed(&mut self) -> bool;
}
