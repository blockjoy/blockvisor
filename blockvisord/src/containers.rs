use anyhow::{Ok, Result};
use async_trait::async_trait;
use firec::Machine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tracing::info;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum ServiceStatus {
    Enabled,
    Disabled,
}

#[derive(Deserialize, Serialize, PartialEq, Clone, Copy, Debug)]
pub enum ContainerStatus {
    Created,
    Started,
    Stopped,
    Deleted,
}

#[async_trait]
pub trait NodeContainer {
    /// Creates a new container with `id`.
    /// TODO: machine_index is a hack. Remove after demo.
    async fn create(id: &str, machine_index: usize) -> Result<Self>
    where
        Self: Sized;

    /// Returns the container's `id`.
    fn id(&self) -> &str;

    /// Starts the container.
    async fn start(&mut self) -> Result<()>;

    /// Returns the state of the container.
    async fn state(&self) -> Result<ContainerStatus>;

    /// Kills the running container.
    async fn kill(&mut self) -> Result<()>;

    /// Deletes the container.
    async fn delete(&mut self) -> Result<()>;
}

pub trait NodeRegistry {
    fn contains(id: &str) -> bool;
    fn get(id: &str) -> Result<Box<dyn NodeContainer>>;
}

pub struct LinuxNode {
    id: String,
    machine: Machine<'static>,
}

// FIXME: Hardcoding everything for now.
const KERNEL_PATH: &str = "/var/demo/debian-vmlinux";
const ROOT_FS: &str = "/var/demo/debian.ext4";
const CHROOT_PATH: &str = "/var/demo/helium";
const FC_BIN_PATH: &str = "/usr/bin/firecracker";
const FC_SOCKET_PATH: &str = "/firecracker.socket";

#[async_trait]
impl NodeContainer for LinuxNode {
    async fn create(id: &str, machine_index: usize) -> Result<Self> {
        let jailer = firec::config::Jailer::builder()
            .chroot_base_dir(Path::new(CHROOT_PATH))
            .exec_file(Path::new(FC_BIN_PATH))
            .build();

        let root_drive = firec::config::Drive::builder("root", Path::new(ROOT_FS))
            .is_root_device(true)
            .build();

        let kernel_args = Some(format!(
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on \
            ip=74.50.82.8{}::74.50.82.81:255.255.255.240::eth0:on",
            machine_index + 3,
        ));

        let if_name = format!("bv{}", machine_index);
        let iface = firec::config::network::Interface::new("eth0", if_name);

        let machine_cfg = firec::config::Machine::builder()
            .vcpu_count(1)
            .mem_size_mib(8192)
            .build();

        let config = firec::config::Config::builder(Path::new(KERNEL_PATH))
            .vm_id(Uuid::parse_str(id)?)
            .jailer_cfg(Some(jailer))
            .kernel_args(kernel_args)
            .machine_cfg(machine_cfg)
            .add_drive(root_drive)
            .add_network_interface(iface)
            .socket_path(Path::new(FC_SOCKET_PATH))
            .build();
        let machine = firec::Machine::new(config).await?;

        Ok(Self {
            id: id.to_string(),
            machine,
        })
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn start(&mut self) -> Result<()> {
        self.machine.start().await.map_err(Into::into)
    }

    async fn state(&self) -> Result<ContainerStatus> {
        unimplemented!()
    }

    async fn kill(&mut self) -> Result<()> {
        self.machine.shutdown().await.map_err(Into::into)
    }

    async fn delete(&mut self) -> Result<()> {
        unimplemented!()
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DummyNode {
    pub id: String,
    pub state: ContainerStatus,
}

#[async_trait]
impl NodeContainer for DummyNode {
    async fn create(id: &str, _machine_index: usize) -> Result<Self> {
        info!("Creating node: {}", id);
        let node = Self {
            id: id.to_owned(),
            state: ContainerStatus::Created,
        };
        let contents = toml::to_string(&node)?;
        fs::write(format!("/tmp/{}.txt", id), &contents)?;
        Ok(node)
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn start(&mut self) -> Result<()> {
        info!("Starting node: {}", self.id());
        self.state = ContainerStatus::Started;
        let contents = toml::to_string(&self)?;
        fs::write(format!("/tmp/{}.txt", self.id), &contents)?;
        Ok(())
    }

    async fn state(&self) -> Result<ContainerStatus> {
        Ok(self.state)
    }

    async fn kill(&mut self) -> Result<()> {
        info!("Killing node: {}", self.id());
        self.state = ContainerStatus::Stopped;
        let contents = toml::to_string(&self)?;
        fs::write(format!("/tmp/{}.txt", self.id), &contents)?;
        Ok(())
    }

    async fn delete(&mut self) -> Result<()> {
        info!("Deleting node: {}", self.id());
        self.kill().await?;
        fs::remove_file(format!("/tmp/{}.txt", self.id))?;
        Ok(())
    }
}

pub struct DummyNodeRegistry {}

impl NodeRegistry for DummyNodeRegistry {
    fn contains(id: &str) -> bool {
        Path::new(&format!("/tmp/{}.txt", id)).exists()
    }

    fn get(id: &str) -> Result<Box<dyn NodeContainer>> {
        let node = fs::read_to_string(format!("/tmp/{}.txt", id))?;
        let node: DummyNode = toml::from_str(&node)?;

        Ok(Box::new(DummyNode {
            id: id.to_string(),
            state: node.state,
        }))
    }
}
