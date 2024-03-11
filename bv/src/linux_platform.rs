/// Default Platform Abstraction Layer implementation for Linux.
use crate::{
    config,
    config::SharedConfig,
    firecracker_machine, node_connection,
    node_data::NodeData,
    nodes_manager::NodesDataCache,
    pal,
    pal::{AvailableResources, NetInterface, Pal},
    services,
    services::blockchain::DATA_FILE,
    utils, BV_VAR_PATH,
};
use async_trait::async_trait;
use bv_utils::{cmd::run_cmd, exp_backoff_timeout};
use core::fmt;
use eyre::{anyhow, bail, Context, Result};
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Instant,
};
use sysinfo::{DiskExt, System, SystemExt};
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
    pub async fn new_with_config() -> Result<(Self, config::Config)> {
        let bv_root = bv_root();
        let config = config::Config::load(&bv_root).await?;
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
        Ok((
            Self {
                bv_root,
                babel_path,
                job_runner_path,
            },
            config,
        ))
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
        prepare_data_image(&self.bv_root, node_data).await?;
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

    type RecoveryBackoff = RecoveryBackoff;
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

const MAX_START_TRIES: u32 = 3;
const START_RECOVERY_BACKOFF_BASE_MS: u64 = 15_000;
const MAX_STOP_TRIES: u32 = 3;
const STOP_RECOVERY_BACKOFF_BASE_MS: u64 = 45_000;
const MAX_RECONNECT_TRIES: u32 = 3;
const RECONNECT_RECOVERY_BACKOFF_BASE_MS: u64 = 5_000;

#[derive(Debug, Default)]
pub struct RecoveryBackoff {
    reconnect: u32,
    stop: u32,
    start: u32,
    backoff_time: Option<Instant>,
}

impl pal::RecoverBackoff for RecoveryBackoff {
    fn backoff(&self) -> bool {
        if let Some(backoff_time) = &self.backoff_time {
            Instant::now() < *backoff_time
        } else {
            false
        }
    }

    fn reset(&mut self) {
        *self = Default::default();
    }

    fn start_failed(&mut self) -> bool {
        self.update_backoff_time(START_RECOVERY_BACKOFF_BASE_MS, self.start);
        self.start += 1;
        self.start >= MAX_START_TRIES
    }

    fn stop_failed(&mut self) -> bool {
        self.update_backoff_time(STOP_RECOVERY_BACKOFF_BASE_MS, self.stop);
        self.stop += 1;
        self.stop >= MAX_STOP_TRIES
    }

    fn reconnect_failed(&mut self) -> bool {
        self.update_backoff_time(RECONNECT_RECOVERY_BACKOFF_BASE_MS, self.reconnect);
        self.reconnect += 1;
        self.reconnect >= MAX_RECONNECT_TRIES
    }
}

impl RecoveryBackoff {
    fn update_backoff_time(&mut self, backoff_base_ms: u64, counter: u32) {
        self.backoff_time = Some(Instant::now() + exp_backoff_timeout(backoff_base_ms, counter));
    }
}
