use crate::utils::run_cmd;
use anyhow::Result;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::IpAddr;

const BRIDGE_IFACE: &str = "bvbr0";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NetworkInterface {
    pub name: String,
    pub ip: IpAddr,
    pub gateway: IpAddr,
}

impl NetworkInterface {
    /// Creates the new network interface and add it to our bridge.
    ///
    /// The `ip` is not assigned on the host but rather by the API.
    pub async fn create(name: String, ip: IpAddr, gateway: IpAddr) -> Result<Self> {
        // First create the interface.
        run_cmd("ip", ["tuntap", "add", &name, "mode", "tap"]).await?;

        // Set bridge as the interface's master.
        if let Err(e) = run_cmd("ip", ["link", "set", &name, "master", BRIDGE_IFACE])
            // Start the interface.
            .and_then(|_| run_cmd("ip", ["link", "set", &name, "up"]))
            .await
        {
            // Clean up the interface if we failed to set it up completely.
            delete_interface(&name).await?;

            return Err(e);
        }

        Ok(Self { name, ip, gateway })
    }

    /// Delete the network interface.
    pub async fn delete(self) -> Result<()> {
        delete_interface(&self.name).await
    }
}

async fn delete_interface(name: &str) -> Result<()> {
    run_cmd("ip", ["link", "delete", name, "type", "tuntap"]).await
}

impl fmt::Display for NetworkInterface {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.ip)
    }
}
