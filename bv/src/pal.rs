use crate::config::SharedConfig;
/// Platform Abstraction Layer is a helper module which goal is to increase testability of BV.
/// Original intention is testability, not portability, nevertheless it may be useful if such requirement appear.
///
/// It defines `Pal` trait which is top level abstraction that contains definitions of sub layers.
///
use anyhow::Result;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt::Debug;
use std::net::IpAddr;
use std::path::Path;

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
