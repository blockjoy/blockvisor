/// Platform Abstraction Layer is a helper module which goal is to increase testability of BV.
/// Original intention is testability, not portability, nevertheless it may be useful if such requirement appear.
///
/// It defines `Pal` trait which is top level abstraction and its default implementation `LinuxPlatform`.
///
use crate::utils::run_cmd;
use anyhow::Result;
use async_trait::async_trait;
use core::fmt;
use futures_util::TryFutureExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::net::IpAddr;

/// Platform Abstraction Layer - trait used to detach business logic form platform specifics, so it
/// can be easily tested.
#[async_trait]
pub trait Pal {
    /// Type representing network interface. It is required to be Serialize/Deserialize
    /// since it's going to be part of node data.
    type NetInterface: NetInterface + Serialize + DeserializeOwned + Debug;
    /// Creates the new network interface and add it to our bridge.
    /// The `ip` is not assigned on the host but rather by the API.
    async fn create_net_interface(
        &self,
        name: String,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<Self::NetInterface>;
}

#[async_trait]
pub trait NetInterface {
    fn name(&self) -> &String;
    fn ip(&self) -> &IpAddr;
    fn gateway(&self) -> &IpAddr;

    /// Remaster the network interface.
    async fn remaster(self) -> Result<()>;
    /// Delete the network interface.
    async fn delete(self) -> Result<()>;
}

#[derive(Debug)]
pub struct LinuxPlatform;

#[async_trait]
impl Pal for LinuxPlatform {
    type NetInterface = LinuxNetInterface;

    /// Creates the new network interface and add it to our bridge.
    ///
    /// The `ip` is not assigned on the host but rather by the API.
    async fn create_net_interface(
        &self,
        name: String,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<Self::NetInterface> {
        // First create the interface.
        run_cmd("ip", ["tuntap", "add", &name, "mode", "tap"]).await?;

        // Then link it to master
        remaster(&name).await?;

        Ok(LinuxNetInterface { name, ip, gateway })
    }
}

const BRIDGE_IFACE: &str = "bvbr0";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct LinuxNetInterface {
    pub name: String,
    pub ip: IpAddr,
    pub gateway: IpAddr,
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
    async fn remaster(self) -> Result<()> {
        remaster(&self.name).await
    }

    /// Delete the network interface.
    async fn delete(self) -> Result<()> {
        delete(&self.name).await
    }
}

async fn remaster(name: &str) -> Result<()> {
    // Set bridge as the interface's master.
    if let Err(e) = run_cmd("ip", ["link", "set", name, "master", BRIDGE_IFACE])
        // Start the interface.
        .and_then(|_| run_cmd("ip", ["link", "set", name, "up"]))
        .await
    {
        // Clean up the interface if we failed to set it up completely.
        delete(name).await?;

        return Err(e);
    }

    Ok(())
}

async fn delete(name: &str) -> Result<()> {
    run_cmd("ip", ["link", "delete", name, "type", "tuntap"]).await
}

impl fmt::Display for LinuxNetInterface {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.ip)
    }
}
