use crate::hosts::HostInfo;
use crate::pal::AvailableResources;
use crate::services::blockchain::DATA_FILE;
/// Default Platform Abstraction Layer implementation for Linux.
use crate::{
    config,
    config::SharedConfig,
    firecracker_machine, node_connection,
    node_data::NodeData,
    pal::{NetInterface, Pal},
    services, BV_VAR_PATH,
};
use async_trait::async_trait;
use babel_api::metadata::Requirements;
use bv_utils::cmd::run_cmd;
use core::fmt;
use eyre::{anyhow, bail, Context, Result};
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
};
use uuid::Uuid;

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
        .ok_or_else(|| anyhow!("invalid current binary dir - has no parent"))?
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
        config: &SharedConfig,
    ) -> Result<Self::NetInterface> {
        let name = format!("bv{index}");
        // First create the interface.
        run_cmd("ip", ["tuntap", "add", &name, "mode", "tap"]).await?;

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

    type NodeConnection = node_connection::NodeConnection;
    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection {
        node_connection::new(&self.build_vm_data_path(node_id))
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

    fn available_resources(
        &self,
        requirements: &[(Uuid, Requirements)],
    ) -> Result<AvailableResources> {
        let host_info = HostInfo::collect()?;
        let mut available_mem_size_mb = host_info.memory_bytes / 1_000_000;
        let mut available_vcpu_count = host_info.cpu_count;
        let mut available_disk_space =
            bv_utils::system::available_disk_space_by_path(&self.bv_root.join(BV_VAR_PATH))?;

        for (node_id, requirements) in requirements {
            let data_img_path = self.build_vm_data_path(*node_id).join(DATA_FILE);
            let actual_data_size = data_img_path
                .metadata()
                .with_context(|| format!("can't check size of '{}'", data_img_path.display()))?
                .len();
            let declared_data_size = requirements.disk_size_gb * 1_000_000_000;
            if declared_data_size > actual_data_size {
                available_disk_space -= declared_data_size - actual_data_size;
            }
            available_mem_size_mb -= requirements.mem_size_mb;
            available_vcpu_count -= requirements.vcpu_count;
        }
        Ok(AvailableResources {
            vcpu_count: available_vcpu_count,
            mem_size_mb: available_mem_size_mb,
            disk_size_gb: available_disk_space / 1_000_000_000,
        })
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
        remaster(&self.name, &self.bridge_ifa).await
    }

    /// Delete the network interface.
    async fn delete(&self) -> Result<()> {
        delete(&self.name).await
    }
}

async fn remaster(name: &str, bridge_ifa: &str) -> Result<()> {
    // Try to create interface if it's not present (possibly after host reboot)
    let _ = run_cmd("ip", ["tuntap", "add", name, "mode", "tap"]).await;

    // Set bridge as the interface's master.
    run_cmd("ip", ["link", "set", name, "master", bridge_ifa])
        // Start the interface.
        .and_then(|_| run_cmd("ip", ["link", "set", name, "up"]))
        .await?;
    Ok(())
}

async fn delete(name: &str) -> Result<()> {
    // try to delete only if exists
    if run_cmd("ip", ["link", "show", name]).await.is_ok() {
        run_cmd("ip", ["link", "delete", name, "type", "tuntap"]).await?;
    }
    Ok(())
}

impl fmt::Display for LinuxNetInterface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ip)
    }
}
