/// Default Platform Abstraction Layer implementation for Linux.
use crate::{
    bare_machine, config,
    config::SharedConfig,
    linux_platform,
    node_data::NodeData,
    nodes_manager::NodesDataCache,
    pal::{AvailableResources, BabelClient, NetInterface, NodeConnection, Pal},
    services, utils, BV_VAR_PATH,
};
use async_trait::async_trait;
use core::fmt;
use eyre::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::{
    net::IpAddr,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};
use sysinfo::{DiskExt, System, SystemExt};
use uuid::Uuid;

#[derive(Debug)]
pub struct LinuxBarePlatform(linux_platform::LinuxPlatform);

impl Deref for LinuxBarePlatform {
    type Target = linux_platform::LinuxPlatform;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for LinuxBarePlatform {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl LinuxBarePlatform {
    pub async fn new() -> Result<Self> {
        Ok(Self(linux_platform::LinuxPlatform::new().await?))
    }
}

#[async_trait]
impl Pal for LinuxBarePlatform {
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

    type NodeConnection = BareNodeConnection;
    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection {
        BareNodeConnection::new(&self.build_vm_data_path(node_id))
    }

    type VirtualMachine = bare_machine::BareMachine;

    async fn create_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        bare_machine::create(&self.bv_root, node_data).await
    }

    async fn attach_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        bare_machine::attach(&self.bv_root, node_data).await
    }

    fn build_vm_data_path(&self, id: Uuid) -> PathBuf {
        self.bv_root
            .join(BV_VAR_PATH)
            .join("bare")
            .join(id.to_string())
    }

    fn available_resources(&self, nodes_data_cache: &NodesDataCache) -> Result<AvailableResources> {
        let mut sys = System::new_all();
        sys.refresh_all();
        let (available_mem_size_mb, available_vcpu_count) = nodes_data_cache.iter().fold(
            (sys.total_memory() / 1_000_000, sys.cpus().len()),
            |(available_mem_size_mb, available_vcpu_count), (_, data)| {
                (
                    available_mem_size_mb - data.requirements.mem_size_mb,
                    available_vcpu_count - data.requirements.vcpu_count,
                )
            },
        );
        let available_disk_space =
            bv_utils::system::find_disk_by_path(&sys, &self.bv_root.join(BV_VAR_PATH))
                .map(|disk| disk.available_space())
                .ok_or_else(|| anyhow!("Cannot get available disk space"))?
                - utils::used_disk_space_correction(&self.bv_root, nodes_data_cache)?;
        Ok(AvailableResources {
            vcpu_count: available_vcpu_count,
            mem_size_mb: available_mem_size_mb,
            disk_size_gb: available_disk_space / 1_000_000_000,
        })
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
        Ok(())
    }

    /// Delete the network interface.
    async fn delete(&self) -> Result<()> {
        Ok(())
    }
}

impl fmt::Display for LinuxNetInterface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ip)
    }
}

#[derive(Debug)]
pub struct BareNodeConnection {}

impl BareNodeConnection {
    fn new(_vm_path: &Path) -> Self {
        Self {}
    }
}

#[async_trait]
impl NodeConnection for BareNodeConnection {
    async fn setup(&mut self) -> Result<()> {
        todo!()
    }

    async fn attach(&mut self) -> Result<()> {
        todo!()
    }

    fn close(&mut self) {
        todo!()
    }

    fn is_closed(&self) -> bool {
        todo!()
    }

    fn mark_broken(&mut self) {
        todo!()
    }

    fn is_broken(&self) -> bool {
        todo!()
    }

    async fn test(&mut self) -> Result<()> {
        todo!()
    }

    async fn babel_client(&mut self) -> Result<&mut BabelClient> {
        todo!()
    }
}
