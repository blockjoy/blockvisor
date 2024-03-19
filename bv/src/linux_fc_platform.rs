/// Default Platform Abstraction Layer implementation for Linux.
use crate::{
    config,
    config::SharedConfig,
    firecracker_machine,
    firecracker_machine::FC_BIN_NAME,
    linux_platform, node_connection,
    node_data::NodeData,
    nodes_manager::NodesDataCache,
    pal::{AvailableResources, NetInterface, Pal},
    services,
    services::blockchain::DATA_FILE,
    utils,
};
use async_trait::async_trait;
use bv_utils::cmd::run_cmd;
use core::fmt;
use eyre::{Context, Result};
use filesize::PathExt;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    net::IpAddr,
    path::{Path, PathBuf},
};
use sysinfo::Pid;
use tracing::debug;
use uuid::Uuid;

#[derive(Debug)]
pub struct LinuxFcPlatform(linux_platform::LinuxPlatform);

impl LinuxFcPlatform {
    pub async fn new() -> Result<Self> {
        Ok(Self(linux_platform::LinuxPlatform::new().await?))
    }
}

/// Create new data drive in chroot location, or copy it from cache
pub async fn prepare_data_image<P>(bv_root: &Path, data: &NodeData<P>) -> Result<()> {
    // allocate new image on location, if it's not there yet
    let vm_data_dir = firecracker_machine::build_vm_data_path(bv_root, data.id);
    let data_file_path = vm_data_dir.join(DATA_FILE);
    if !data_file_path.exists() {
        tokio::fs::create_dir_all(&vm_data_dir).await?;
        let disk_size_gb = data.requirements.disk_size_gb;
        let gb = &format!("{disk_size_gb}GB");
        run_cmd(
            "fallocate",
            [OsStr::new("-l"), OsStr::new(gb), data_file_path.as_os_str()],
        )
        .await?;
        run_cmd("mkfs.ext4", [data_file_path.as_os_str()]).await?;
    }

    Ok(())
}

#[async_trait]
impl Pal for LinuxFcPlatform {
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
        node_connection::new(&self.build_vm_data_path(node_id), self.0.babel_path.clone())
    }

    type VirtualMachine = firecracker_machine::FirecrackerMachine;

    async fn create_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        prepare_data_image(&self.0.bv_root, node_data).await?;
        firecracker_machine::create(&self.0.bv_root, node_data).await
    }

    async fn attach_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        firecracker_machine::attach(&self.0.bv_root, node_data).await
    }

    fn get_vm_pids(&self) -> Result<Vec<Pid>> {
        utils::get_all_processes_pids(FC_BIN_NAME)
    }

    fn get_vm_pid(&self, vm_id: Uuid) -> Result<Pid> {
        Ok(utils::get_process_pid(FC_BIN_NAME, &vm_id.to_string())?)
    }

    fn build_vm_data_path(&self, id: Uuid) -> PathBuf {
        firecracker_machine::build_vm_data_path(&self.0.bv_root, id)
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
            let data_img_path = self.build_vm_data_path(*id).join(DATA_FILE);
            let actual_data_size = data_img_path
                .size_on_disk()
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
