/// Platform Abstraction Layer is a helper module which goal is to increase testability of BV.
/// Original intention is testability, not portability, nevertheless it may be useful if such requirement appear.
///
/// It defines `Pal` trait which is top level abstraction that contains definitions of sub layers.
///
use crate::{config::SharedConfig, node_data::NodeData};
use async_trait::async_trait;
use eyre::Result;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fmt::Debug,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tonic::{
    codegen::InterceptedService,
    service::Interceptor,
    transport::Channel,
    {Request, Status},
};
use uuid::Uuid;

/// Platform Abstraction Layer - trait used to detach business logic form platform specifics, so it
/// can be easily tested.
#[async_trait]
pub trait Pal {
    /// Root directory for all BV paths. Instead abstracting whole file system, just let tests
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
    /// Build path to VM data directory, a place where kernel and other VM related data are stored.
    fn build_vm_data_path(&self, id: Uuid) -> PathBuf;
}

#[async_trait]
pub trait NetInterface {
    fn name(&self) -> &String;
    fn ip(&self) -> &IpAddr;
    fn gateway(&self) -> &IpAddr;

    /// Remaster the network interface.
    async fn remaster(&self) -> Result<()>;
    /// Delete the network interface.
    async fn delete(self) -> Result<()>;
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

pub struct DefaultTimeout(pub Duration);

impl Interceptor for DefaultTimeout {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if request.metadata().get("grpc-timeout").is_none() {
            // set default timeout if not set yet
            request.set_timeout(self.0);
        }
        Ok(request)
    }
}

pub type BabelClient =
    babel_api::babel::babel_client::BabelClient<InterceptedService<Channel, DefaultTimeout>>;
pub type BabelSupClient = babel_api::babelsup::babel_sup_client::BabelSupClient<
    InterceptedService<Channel, DefaultTimeout>,
>;

#[async_trait]
pub trait NodeConnection {
    /// Open node connection with given `max_delay`.
    async fn open(&mut self, max_delay: Duration) -> Result<()>;
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
    async fn test(&self) -> Result<()>;
    /// Get reference to Babel rpc client. Try to reestablish connection if it's necessary.
    async fn babelsup_client(&mut self) -> Result<&mut BabelSupClient>;
    /// Get reference to BabelSup rpc client. Try to reestablish connection if it's necessary.
    async fn babel_client(&mut self) -> Result<&mut BabelClient>;
}

#[derive(Debug, PartialEq)]
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
    async fn delete(mut self) -> Result<()>;
    /// Request for graceful shutdown of the VM.
    async fn shutdown(&mut self) -> Result<()>;
    /// Forcefully shutdown the VM.
    async fn force_shutdown(&mut self) -> Result<()>;
    /// Start the VM.
    async fn start(&mut self) -> Result<()>;
}
