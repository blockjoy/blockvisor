/// Default Platform Abstraction Layer implementation for Linux.
use crate::pal::{NetInterface, Pal};
use crate::utils::run_cmd;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use core::fmt;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LinuxPlatform {
    bv_root: PathBuf,
    babel_path: PathBuf,
}

const ENV_BV_ROOT_KEY: &str = "BV_ROOT";

pub fn bv_root() -> PathBuf {
    PathBuf::from(std::env::var(ENV_BV_ROOT_KEY).unwrap_or_else(|_| "/".to_string()))
}

impl LinuxPlatform {
    pub fn new() -> Result<Self> {
        let babel_path = fs::canonicalize(
            std::env::current_exe().with_context(|| "failed to get current binary path")?,
        )
        .with_context(|| "non canonical current binary path")?
        .parent()
        .with_context(|| "invalid current binary dir - has no parent")?
        .join("../../babel/bin/babel");
        if !babel_path.exists() {
            bail!(
                "babel binary bundled with BV not found: {}",
                babel_path.to_string_lossy()
            )
        }
        Ok(Self {
            bv_root: bv_root(),
            babel_path,
        })
    }
}

#[async_trait]
impl Pal for LinuxPlatform {
    fn bv_root(&self) -> &Path {
        self.bv_root.as_path()
    }

    fn babel_path(&self) -> &Path {
        self.babel_path.as_path()
    }

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
