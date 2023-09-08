/// Default Platform Abstraction Layer implementation for Linux.
use crate::{
    config::SharedConfig,
    firecracker_machine, node_connection,
    node_data::NodeData,
    pal::{NetInterface, Pal},
    services, BV_VAR_PATH,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bv_utils::cmd::run_cmd;
use core::fmt;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const BRIDGE_IFACE: &str = "bvbr0";
const ENV_BV_ROOT_KEY: &str = "BV_ROOT";

#[derive(Debug)]
pub struct LinuxPlatform {
    bv_root: PathBuf,
    babel_path: PathBuf,
    job_runner_path: PathBuf,
}

pub fn bv_root() -> PathBuf {
    PathBuf::from(std::env::var(ENV_BV_ROOT_KEY).unwrap_or_else(|_| "/".to_string()))
}

impl LinuxPlatform {
    pub fn new() -> Result<Self> {
        let babel_dir = fs::canonicalize(
            std::env::current_exe().with_context(|| "failed to get current binary path")?,
        )
        .with_context(|| "non canonical current binary path")?
        .parent()
        .with_context(|| "invalid current binary dir - has no parent")?
        .join("../../babel/bin");
        let babel_path = babel_dir.join("babel");
        if !babel_path.exists() {
            bail!(
                "babel binary bundled with BV not found: {}",
                babel_path.display()
            )
        }
        let job_runner_path = babel_dir.join("babel_job_runner");
        if !job_runner_path.exists() {
            bail!(
                "job runner binary bundled with BV not found: {}",
                job_runner_path.display()
            )
        }
        Ok(Self {
            bv_root: bv_root(),
            babel_path,
            job_runner_path,
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

    fn job_runner_path(&self) -> &Path {
        self.job_runner_path.as_path()
    }

    type NetInterface = LinuxNetInterface;

    /// Creates the new network interface and add it to our bridge.
    ///
    /// The `ip` is not assigned on the host but rather by the API.
    async fn create_net_interface(
        &self,
        index: u32,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<Self::NetInterface> {
        let name = format!("bv{index}");
        // First create the interface.
        run_cmd("ip", ["tuntap", "add", &name, "mode", "tap"]).await?;

        Ok(LinuxNetInterface { name, ip, gateway })
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

    type NodeConnection = node_connection::NodeConnection;
    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection {
        node_connection::new(&self.bv_root.join(BV_VAR_PATH), node_id)
    }

    type VirtualMachine = firecracker_machine::FirecrackerMachine;

    async fn create_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        firecracker_machine::create(&self.bv_root, node_data).await
    }

    async fn attach_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        firecracker_machine::attach(&self.bv_root, node_data).await
    }

    fn build_vm_data_path(&self, id: Uuid) -> PathBuf {
        firecracker_machine::build_vm_data_path(&self.bv_root, id)
    }
}

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
    async fn remaster(&self) -> Result<()> {
        remaster(&self.name).await
    }

    /// Delete the network interface.
    async fn delete(self) -> Result<()> {
        delete(&self.name).await
    }
}

async fn remaster(name: &str) -> Result<()> {
    // Try to create interface if it's not present (possibly after host reboot)
    let _ = run_cmd("ip", ["tuntap", "add", name, "mode", "tap"]).await;

    // Set bridge as the interface's master.
    run_cmd("ip", ["link", "set", name, "master", BRIDGE_IFACE])
        // Start the interface.
        .and_then(|_| run_cmd("ip", ["link", "set", name, "up"]))
        .await?;
    Ok(())
}

async fn delete(name: &str) -> Result<()> {
    run_cmd("ip", ["link", "delete", name, "type", "tuntap"]).await?;
    Ok(())
}

impl fmt::Display for LinuxNetInterface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ip)
    }
}
