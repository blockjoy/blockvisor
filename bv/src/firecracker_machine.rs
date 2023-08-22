use crate::services::kernel::KernelService;
use crate::{
    node_data::NodeData,
    pal,
    services::cookbook::{CookbookService, DATA_FILE, ROOT_FS_FILE},
    utils::get_process_pid,
    BV_VAR_PATH,
};
use anyhow::{bail, Result};
use async_trait::async_trait;
use firec::{config::JailerMode, Machine, MachineState};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const FC_BIN_NAME: &str = "firecracker";
const FC_BIN_PATH: &str = "usr/bin/firecracker";
const FC_SOCKET_PATH: &str = "/firecracker.socket";
const VSOCK_GUEST_CID: u32 = 3;
const MAX_KERNEL_ARGS_LEN: usize = 1024;
pub const VSOCK_PATH: &str = "vsock.socket";

#[derive(Debug)]
pub struct FirecrackerMachine {
    machine: Machine<'static>,
}

pub async fn create(
    bv_root: &Path,
    node_data: &NodeData<impl pal::NetInterface>,
) -> Result<FirecrackerMachine> {
    Ok(FirecrackerMachine {
        machine: Machine::create(create_config(bv_root, node_data).await?).await?,
    })
}

pub async fn attach(
    bv_root: &Path,
    node_data: &NodeData<impl pal::NetInterface>,
) -> Result<FirecrackerMachine> {
    Ok(FirecrackerMachine {
        machine: Machine::connect(
            create_config(bv_root, node_data).await?,
            get_process_pid(FC_BIN_NAME, &node_data.id.to_string()).ok(),
        )
        .await,
    })
}

pub fn build_vm_data_path(bv_root: &Path, id: Uuid) -> PathBuf {
    bv_root
        .join(BV_VAR_PATH)
        .join(FC_BIN_NAME)
        .join(id.to_string())
        .join("root")
}

#[async_trait]
impl pal::VirtualMachine for FirecrackerMachine {
    fn state(&self) -> pal::VmState {
        match self.machine.state() {
            MachineState::SHUTOFF => pal::VmState::SHUTOFF,
            MachineState::RUNNING => pal::VmState::RUNNING,
        }
    }

    async fn delete(self) -> Result<()> {
        self.machine.delete().await?;
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.machine.shutdown().await?;
        Ok(())
    }

    async fn force_shutdown(&mut self) -> Result<()> {
        self.machine.force_shutdown().await?;
        Ok(())
    }

    async fn start(&mut self) -> Result<()> {
        self.machine.start().await?;
        Ok(())
    }
}

async fn create_config(
    bv_root: &Path,
    data: &NodeData<impl pal::NetInterface>,
) -> Result<firec::config::Config<'static>> {
    let kernel_args = format!(
        "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on \
            ip={}::{}:255.255.255.240::eth0:on",
        data.network_interface.ip(),
        data.network_interface.gateway(),
    );
    if kernel_args.len() > MAX_KERNEL_ARGS_LEN {
        bail!("too long kernel_args {kernel_args}")
    }
    let iface = firec::config::network::Interface::new(
        data.network_interface.name().clone(),
        "eth0",
        None::<&str>,
    );
    let root_fs_path =
        CookbookService::get_image_download_folder_path(bv_root, &data.image).join(ROOT_FS_FILE);
    let kernel_path = KernelService::get_kernel_path(bv_root, &data.kernel);
    let data_fs_path = build_vm_data_path(bv_root, data.id).join(DATA_FILE);

    let config = firec::config::Config::builder(Some(data.id), kernel_path)
        // Jailer configuration.
        .jailer_cfg()
        .chroot_base_dir(bv_root.join(BV_VAR_PATH))
        .exec_file(bv_root.join(FC_BIN_PATH))
        .mode(JailerMode::Tmux(Some(data.name.clone().into())))
        .build()
        // Machine configuration.
        .machine_cfg()
        .vcpu_count(data.requirements.vcpu_count)
        .mem_size_mib(data.requirements.mem_size_mb as i64)
        .build()
        // Add root drive.
        .add_drive("root", root_fs_path)
        .is_root_device(true)
        .build()
        // Add data drive.
        .add_drive("data", data_fs_path)
        .build()
        // Network configuration.
        .add_network_interface(iface)
        // Rest of the configuration.
        .socket_path(Path::new(FC_SOCKET_PATH))
        .kernel_args(kernel_args)
        .vsock_cfg(VSOCK_GUEST_CID, Path::new("/").join(VSOCK_PATH))
        .build();

    Ok(config)
}
