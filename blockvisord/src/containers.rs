use anyhow::{bail, Result};
use async_trait::async_trait;
use firec::config::JailerMode;
use firec::Machine;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use sysinfo::{PidExt, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use tokio::fs;
use tokio::time::sleep;
use tracing::{info, instrument, trace};
use uuid::Uuid;

const CONTAINERS_CONFIG_FILENAME: &str = "containers.toml";

lazy_static::lazy_static! {
    static ref REGISTRY_CONFIG_FILE: PathBuf = home::home_dir()
        .map(|p| p.join(".cache"))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("blockvisor")
        .join(CONTAINERS_CONFIG_FILENAME);
}

#[derive(Clone, Debug)]
pub enum ServiceStatus {
    Enabled,
    Disabled,
}

#[derive(Deserialize, Serialize, PartialEq, Clone, Copy, Debug)]
pub enum ContainerState {
    Created,
    Started,
    Stopped,
    Deleted,
}

#[async_trait]
pub trait NodeContainer {
    /// Creates a new container with `id`.
    /// TODO: machine_index is a hack. Remove after demo.
    async fn create(id: Uuid, network_interface: &NetworkInterface) -> Result<Self>
    where
        Self: Sized;

    /// Checks if container exists on this host.
    async fn exists(id: Uuid) -> bool;

    /// Returns container previously created on this host.
    async fn connect(id: Uuid, network_interface: &NetworkInterface) -> Result<Self>
    where
        Self: Sized;

    /// Returns the container's `id`.
    fn id(&self) -> &Uuid;

    /// Starts the container.
    async fn start(&mut self) -> Result<()>;

    /// Returns the state of the container.
    async fn state(&self) -> Result<ContainerState>;

    /// Kills the running container.
    async fn kill(&mut self) -> Result<()>;

    /// Deletes the container.
    async fn delete(mut self) -> Result<()>;
}

pub struct LinuxNode {
    id: Uuid,
    machine: Machine<'static>,
}

// FIXME: Hardcoding everything for now.
const KERNEL_PATH: &str = "/var/demo/debian-vmlinux";
const ROOT_FS: &str = "/var/demo/debian.ext4";
const CHROOT_PATH: &str = "/var/demo/helium";
const FC_BIN_PATH: &str = "/usr/bin/firecracker";
const FC_BIN_NAME: &str = "firecracker";
const FC_SOCKET_PATH: &str = "/firecracker.socket";

#[async_trait]
impl NodeContainer for LinuxNode {
    #[instrument]
    async fn create(id: Uuid, network_interface: &NetworkInterface) -> Result<Self> {
        let config = LinuxNode::create_config(id, network_interface)?;
        let machine = firec::Machine::create(config).await?;

        Ok(Self { id, machine })
    }

    async fn exists(id: Uuid) -> bool {
        Path::new(&format!("{CHROOT_PATH}/{FC_BIN_NAME}/{id}")).exists()
    }

    #[instrument]
    async fn connect(id: Uuid, network_interface: &NetworkInterface) -> Result<Self> {
        let config = LinuxNode::create_config(id, network_interface)?;
        let cmd = id.to_string();
        let state = match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(pid) => firec::MachineState::RUNNING { pid },
            Err(_) => firec::MachineState::SHUTOFF,
        };
        let machine = firec::Machine::connect(config, state).await;

        Ok(Self { id, machine })
    }

    fn id(&self) -> &Uuid {
        &self.id
    }

    #[instrument(skip(self))]
    async fn start(&mut self) -> Result<()> {
        self.machine.start().await.map_err(Into::into)
    }

    async fn state(&self) -> Result<ContainerState> {
        let cmd = self.id().to_string();
        match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(_) => Ok(ContainerState::Started),
            Err(_) => Ok(ContainerState::Created),
        }
    }

    #[instrument(skip(self))]
    async fn kill(&mut self) -> Result<()> {
        match self.machine.state() {
            firec::MachineState::SHUTOFF => {}
            firec::MachineState::RUNNING { .. } => {
                if let Err(err) = self.machine.shutdown().await {
                    trace!("Shutdown error: {err}");
                } else {
                    sleep(Duration::from_secs(10)).await;
                }

                if let Err(err) = self.machine.force_shutdown().await {
                    trace!("Forced shutdown error: {err}");
                }
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    async fn delete(mut self) -> Result<()> {
        self.machine.delete().await.map_err(Into::into)
    }
}

impl LinuxNode {
    fn create_config(
        id: Uuid,
        network_interface: &NetworkInterface,
    ) -> Result<firec::config::Config<'static>> {
        let jailer = firec::config::Jailer::builder()
            .chroot_base_dir(Path::new(CHROOT_PATH))
            .exec_file(Path::new(FC_BIN_PATH))
            .mode(JailerMode::Daemon)
            .build();

        let root_drive = firec::config::Drive::builder("root", Path::new(ROOT_FS))
            .is_root_device(true)
            .build();
        let kernel_args = Some(format!(
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on \
            ip={}::74.50.82.81:255.255.255.240::eth0:on",
            network_interface.ip,
        ));

        let iface = firec::config::network::Interface::new("eth0", network_interface.name.clone());

        let machine_cfg = firec::config::Machine::builder()
            .vcpu_count(1)
            .mem_size_mib(8192)
            .build();

        let config = firec::config::Config::builder(Path::new(KERNEL_PATH))
            .vm_id(id)
            .jailer_cfg(Some(jailer))
            .kernel_args(kernel_args)
            .machine_cfg(machine_cfg)
            .add_drive(root_drive)
            .add_network_interface(iface)
            .socket_path(Path::new(FC_SOCKET_PATH))
            .build()?;

        Ok(config)
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DummyNode {
    pub id: Uuid,
    pub state: ContainerState,
}

#[async_trait]
impl NodeContainer for DummyNode {
    async fn create(id: Uuid, _network_interface: &NetworkInterface) -> Result<Self> {
        info!("Creating node: {}", id);
        let node = Self {
            id,
            state: ContainerState::Created,
        };
        let contents = toml::to_string(&node)?;
        fs::write(format!("/tmp/{}.txt", id), &contents).await?;
        Ok(node)
    }

    async fn exists(id: Uuid) -> bool {
        Path::new(&format!("/tmp/{}.txt", id)).exists()
    }

    async fn connect(id: Uuid, _network_interface: &NetworkInterface) -> Result<Self> {
        let node = fs::read_to_string(format!("/tmp/{}.txt", id)).await?;
        let node: DummyNode = toml::from_str(&node)?;

        Ok(DummyNode {
            id,
            state: node.state,
        })
    }

    fn id(&self) -> &Uuid {
        &self.id
    }

    async fn start(&mut self) -> Result<()> {
        info!("Starting node: {}", self.id());
        self.state = ContainerState::Started;
        let contents = toml::to_string(&self)?;
        fs::write(format!("/tmp/{}.txt", self.id), &contents).await?;
        Ok(())
    }

    async fn state(&self) -> Result<ContainerState> {
        Ok(self.state)
    }

    async fn kill(&mut self) -> Result<()> {
        info!("Killing node: {}", self.id());
        self.state = ContainerState::Stopped;
        let contents = toml::to_string(&self)?;
        fs::write(format!("/tmp/{}.txt", self.id), &contents).await?;
        Ok(())
    }

    async fn delete(mut self) -> Result<()> {
        info!("Deleting node: {}", self.id());
        self.kill().await?;
        fs::remove_file(format!("/tmp/{}.txt", self.id)).await?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Containers {
    pub containers: HashMap<Uuid, ContainerData>,
    machine_index: Arc<Mutex<u32>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ContainerData {
    pub id: Uuid,
    pub chain: String,
    pub state: ContainerState,
}

impl Containers {
    pub async fn load() -> Result<Containers> {
        info!(
            "Reading containers config: {}",
            REGISTRY_CONFIG_FILE.display()
        );
        let config = fs::read_to_string(&*REGISTRY_CONFIG_FILE).await?;
        Ok(toml::from_str(&config)?)
    }

    pub async fn save(&self) -> Result<()> {
        info!(
            "Writing containers config: {}",
            REGISTRY_CONFIG_FILE.display()
        );
        let config = toml::Value::try_from(self)?;
        let config = toml::to_string(&config)?;
        fs::write(&*REGISTRY_CONFIG_FILE, &*config).await?;
        Ok(())
    }

    pub fn exists() -> bool {
        Path::new(&*REGISTRY_CONFIG_FILE).exists()
    }

    /// Get the next machine index and increment it.
    pub fn next_network_interface(&self) -> NetworkInterface {
        let mut machine_index = self.machine_index.lock().expect("lock poisoned");

        let idx_bytes = machine_index.to_be_bytes();
        let iface = NetworkInterface {
            name: format!("bv{}", *machine_index),
            // FIXME: Hardcoding address for now.
            ip: IpAddr::V4(Ipv4Addr::new(
                idx_bytes[0] + 74,
                idx_bytes[1] + 50,
                idx_bytes[2] + 82,
                idx_bytes[3] + 83,
            )),
        };
        *machine_index += 1;

        iface
    }
}

#[derive(Debug, Clone)]
pub struct NetworkInterface {
    pub name: String,
    pub ip: IpAddr,
}

/// Get the pid of the running VM process knowing its process name and part of command line.
fn get_process_pid(process_name: &str, cmd: &str) -> Result<i32> {
    let mut sys = System::new();
    // TODO: would be great to save the System and not do a full refresh each time
    sys.refresh_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::everything()));
    let processes: Vec<_> = sys
        .processes_by_name(process_name)
        .filter(|&process| process.cmd().contains(&cmd.to_string()))
        .collect();

    match processes.len() {
        0 => bail!("No {process_name} processes running for id: {cmd}"),
        1 => processes[0].pid().as_u32().try_into().map_err(Into::into),
        _ => bail!("More then 1 {process_name} process running for id: {cmd}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn network_interface_gen() {
        let containers = super::Containers::default();
        let iface = containers.next_network_interface();
        assert_eq!(iface.name, "bv0");
        assert_eq!(
            iface.ip,
            super::IpAddr::V4(super::Ipv4Addr::new(74, 50, 82, 83))
        );

        let iface = containers.next_network_interface();
        assert_eq!(iface.name, "bv1");
        assert_eq!(
            iface.ip,
            super::IpAddr::V4(super::Ipv4Addr::new(74, 50, 82, 84))
        );

        // Let's take the machine_index beyond u8 boundry.
        *containers.machine_index.lock().expect("lock poisoned") = u8::MAX as u32 + 1;
        let iface = containers.next_network_interface();
        assert_eq!(iface.name, format!("bv{}", u8::MAX as u32 + 1));
        assert_eq!(
            iface.ip,
            super::IpAddr::V4(super::Ipv4Addr::new(74, 50, 83, 83))
        );
    }
}
