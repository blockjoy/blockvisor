use anyhow::{bail, Context, Result};
use firec::config::JailerMode;
use firec::Machine;
use std::{future::ready, path::Path, time::Duration};
use sysinfo::{PidExt, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use tokio::{
    fs,
    time::{sleep, timeout},
};
use tracing::{instrument, log::warn, trace};
use uuid::Uuid;
use zbus::{export::futures_util::StreamExt, Connection, Proxy};

use crate::node_data::{NodeData, NodeStatus};

#[derive(Debug)]
pub struct Node {
    pub data: NodeData,
    machine: Machine<'static>,
    babel_proxy: Proxy<'static>,
}

// FIXME: Hardcoding everything for now.
const KERNEL_PATH: &str = "/var/lib/blockvisor/debian-vmlinux";
const ROOT_FS: &str = "/var/lib/blockvisor/debian.ext4";
const CHROOT_PATH: &str = "/var/lib/blockvisor";
const FC_BIN_PATH: &str = "/usr/bin/firecracker";
const FC_BIN_NAME: &str = "firecracker";
const FC_SOCKET_PATH: &str = "/firecracker.socket";
const VSOCK_PATH: &str = "/vsock.socket";
const VSOCK_GUEST_CID: u32 = 3;
const BABEL_VSOCK_PATH: &str = "/var/lib/blockvisor/vsock.socket_42";
const BABEL_BUS_NAME_PREFIX: &str = "com.BlockJoy.Babel.Node";

const BABEL_START_TIMEOUT: Duration = Duration::from_secs(30);
const BABEL_STOP_TIMEOUT: Duration = Duration::from_secs(15);

impl Node {
    /// Creates a new node with `id`.
    #[instrument]
    pub async fn create(data: NodeData, babel_conn: &Connection) -> Result<Self> {
        let babel_proxy = create_babel_proxy(babel_conn, data.id).await?;
        let config = Node::create_config(&data)?;
        let machine = firec::Machine::create(config).await?;
        let workspace_dir = machine.config().jailer_cfg().expect("").workspace_dir();
        let babel_socket_link = workspace_dir.join("vsock.socket_42");
        fs::hard_link(BABEL_VSOCK_PATH, babel_socket_link).await?;
        data.save().await?;

        Ok(Self {
            data,
            machine,
            babel_proxy,
        })
    }

    /// Returns node previously created on this host.
    #[instrument]
    pub async fn connect(data: NodeData, babel_conn: &Connection) -> Result<Self> {
        let babel_proxy = create_babel_proxy(babel_conn, data.id).await?;
        let config = Node::create_config(&data)?;
        let cmd = data.id.to_string();
        let state = match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(pid) => firec::MachineState::RUNNING { pid },
            Err(_) => firec::MachineState::SHUTOFF,
        };
        let machine = firec::Machine::connect(config, state).await;

        Ok(Self {
            data,
            machine,
            babel_proxy,
        })
    }

    /// Returns the node's `id`.
    pub fn id(&self) -> &Uuid {
        &self.data.id
    }

    /// Starts the node.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<()> {
        let mut stream = self
            .babel_proxy
            .receive_owner_changed()
            .await?
            .filter(|unique_name| {
                trace!(
                    "New owner of {}: {:?}",
                    self.babel_proxy.destination(),
                    unique_name
                );
                // Look for the first name-owned event.
                ready(unique_name.is_some())
            });
        self.machine.start().await?;
        timeout(BABEL_START_TIMEOUT, stream.next()).await?;
        self.data.status = NodeStatus::Running;
        self.data.save().await
    }

    /// Returns the status of the node.
    pub async fn status(&self) -> Result<NodeStatus> {
        Ok(self.data.status)
    }

    /// Stops the running node.
    #[instrument(skip(self))]
    pub async fn stop(&mut self) -> Result<()> {
        match self.machine.state() {
            firec::MachineState::SHUTOFF => {}
            firec::MachineState::RUNNING { .. } => {
                let mut stream =
                    self.babel_proxy
                        .receive_owner_changed()
                        .await?
                        .filter(|unique_name| {
                            trace!(
                                "New owner of {}: {:?}",
                                self.babel_proxy.destination(),
                                unique_name
                            );
                            // Look for the first name-lost event.
                            ready(unique_name.is_none())
                        });
                let mut shutdown_success = true;
                if let Err(err) = self.machine.shutdown().await {
                    trace!("Graceful shutdown failed: {err}");

                    // FIXME: Perhaps we should be just bailing out on this one?
                    if let Err(err) = self.machine.force_shutdown().await {
                        trace!("Forced shutdown failed: {err}");
                        shutdown_success = false;
                    }
                }

                if shutdown_success {
                    if let Err(e) = timeout(BABEL_STOP_TIMEOUT, stream.next()).await {
                        warn!("Babel shutdown timeout: {e}");
                    }
                }
            }
        }
        self.data.status = NodeStatus::Stopped;
        self.data.save().await?;

        // FIXME: for some reason firecracker socket is not created by
        // consequent start command if we do not wait a bit here
        sleep(Duration::from_secs(1)).await;

        Ok(())
    }

    /// Deletes the node.
    #[instrument(skip(self))]
    pub async fn delete(self) -> Result<()> {
        self.machine.delete().await?;
        self.data.delete().await
    }

    fn create_config(data: &NodeData) -> Result<firec::config::Config<'static>> {
        let kernel_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on \
            ip={}::74.50.82.81:255.255.255.240::eth0:on blockvisor.node={}",
            data.network_interface.ip, data.id,
        );
        let iface =
            firec::config::network::Interface::new(data.network_interface.name.clone(), "eth0");

        let config = firec::config::Config::builder(Some(data.id), Path::new(KERNEL_PATH))
            // Jailer configuration.
            .jailer_cfg()
            .chroot_base_dir(Path::new(CHROOT_PATH))
            .exec_file(Path::new(FC_BIN_PATH))
            .mode(JailerMode::Tmux(Some(data.name.clone().into())))
            .build()
            // Machine configuration.
            .machine_cfg()
            .vcpu_count(1)
            .mem_size_mib(8192)
            .build()
            // Add root drive.
            .add_drive("root", Path::new(ROOT_FS))
            .is_root_device(true)
            .build()
            // Network configuration.
            .add_network_interface(iface)
            // Rest of the configuration.
            .socket_path(Path::new(FC_SOCKET_PATH))
            .kernel_args(kernel_args)
            .vsock_cfg(VSOCK_GUEST_CID, Path::new(VSOCK_PATH))
            .build();

        Ok(config)
    }
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

async fn create_babel_proxy(conn: &Connection, id: Uuid) -> Result<Proxy<'static>> {
    let babel_bus_name = format!("{}{}", BABEL_BUS_NAME_PREFIX, id);
    Proxy::new(
        conn,
        babel_bus_name,
        "/com/BlockJoy/Babel",
        "com.BlockJoy.Babel",
    )
    .await
    .context("Failed to create Babel proxy")
}
