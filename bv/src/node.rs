use crate::{
    babel_engine::BabelEngine,
    node_connection::NodeConnection,
    node_data::{NodeData, NodeImage, NodeStatus},
    pal::{NetInterface, Pal},
    services::{api::pb::Parameter, cookbook::CookbookService},
    utils::{get_process_pid, run_cmd},
    with_retry, BV_VAR_PATH,
};

use anyhow::{bail, Context, Result};
use babel_api::config::firewall;
use firec::{config::JailerMode, Machine};
use std::{
    ffi::OsStr,
    fmt::Debug,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::{
    fs::{self, DirBuilder, File},
    time::Instant,
};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

const NODE_START_TIMEOUT: Duration = Duration::from_secs(60);
const NODE_RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const NODE_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const NODE_STOPPED_CHECK_INTERVAL: Duration = Duration::from_secs(1);
pub const REGISTRY_CONFIG_DIR: &str = "nodes";
pub const FC_BIN_NAME: &str = "firecracker";
const FC_BIN_PATH: &str = "usr/bin/firecracker";
const FC_SOCKET_PATH: &str = "/firecracker.socket";
pub const ROOT_FS_FILE: &str = "os.img";
pub const KERNEL_FILE: &str = "kernel";
const DATA_FILE: &str = "data.img";
pub const VSOCK_PATH: &str = "vsock.socket";
const VSOCK_GUEST_CID: u32 = 3;
const MAX_KERNEL_ARGS_LEN: usize = 1024;
const MAX_START_TRIES: usize = 3;
const MAX_STOP_TRIES: usize = 3;
const MAX_RECONNECT_TRIES: usize = 3;

pub fn build_registry_dir(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_DIR)
}

#[derive(Debug)]
pub struct Node<P: Pal> {
    pub data: NodeData<<P as Pal>::NetInterface>,
    pub babel_engine: BabelEngine,
    machine: Machine<'static>,
    paths: Paths,
    pal: Arc<P>,
    recovery_counters: RecoveryCounters,
}

#[derive(Debug, Default)]
struct RecoveryCounters {
    reconnect: usize,
    stop: usize,
    start: usize,
}

#[derive(Debug)]
struct Paths {
    bv_root: PathBuf,
    chroot: PathBuf,
    data_dir: PathBuf,
    registry: PathBuf,
    data: PathBuf,
}

impl Paths {
    fn build(bv_root: &Path, id: &Uuid) -> Self {
        Self {
            bv_root: bv_root.to_path_buf(),
            chroot: bv_root.join(BV_VAR_PATH),
            data_dir: bv_root
                .join(BV_VAR_PATH)
                .join(FC_BIN_NAME)
                .join(id.to_string())
                .join("root"),
            registry: build_registry_dir(bv_root),
            data: bv_root.join(BV_VAR_PATH).join(DATA_FILE),
        }
    }
}

impl<P: Pal + Debug> Node<P> {
    /// Creates a new node according to specs.
    #[instrument(skip(data))]
    pub async fn create(pal: Arc<P>, data: NodeData<<P as Pal>::NetInterface>) -> Result<Self> {
        info!("Creating node with data: {data:?}");
        let node_id = data.id;
        let paths = Paths::build(pal.bv_root(), &data.id);
        let _ = tokio::fs::remove_dir_all(&paths.data_dir).await;
        let config = Self::create_config(&paths, &data).await?;
        Self::create_data_image(&paths, data.babel_conf.requirements.disk_size_gb).await?;
        let machine = Machine::create(config).await?;

        data.save(&paths.registry).await?;

        let babel_engine = BabelEngine::new(
            node_id,
            NodeConnection::closed(&paths.chroot, node_id),
            data.babel_conf.clone(),
            data.properties.clone(),
        );
        Ok(Self {
            data,
            babel_engine,
            machine,
            paths,
            pal,
            recovery_counters: Default::default(),
        })
    }

    /// Returns node previously created on this host.
    #[instrument(skip(data))]
    pub async fn attach(pal: Arc<P>, data: NodeData<<P as Pal>::NetInterface>) -> Result<Self> {
        info!("Attaching to node with data: {data:?}");
        let paths = Paths::build(pal.bv_root(), &data.id);
        let config = Self::create_config(&paths, &data).await?;
        let node_id = data.id;
        let cmd = node_id.to_string();
        let (pid, node_conn) = match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(pid) => {
                // Since this is the startup phase it doesn't make sense to wait a long time
                // for the nodes to come online. For that reason we restrict the allowed delay
                // further down.
                debug!("connecting to babel ...");
                let node_conn = Self::connect(
                    &paths.chroot,
                    pal.babel_path(),
                    node_id,
                    NODE_RECONNECT_TIMEOUT,
                )
                .await
                .unwrap_or_else(|err| {
                    warn!(
                        "failed to reestablished babel connection to running node {node_id}: {err}",
                    );
                    NodeConnection::closed(&paths.chroot, node_id)
                });
                (Some(pid), node_conn)
            }
            Err(_) => (None, NodeConnection::closed(&paths.chroot, node_id)),
        };
        let machine = Machine::connect(config, pid).await;
        let babel_engine = BabelEngine::new(
            node_id,
            node_conn,
            data.babel_conf.clone(),
            data.properties.clone(),
        );
        Ok(Self {
            data,
            babel_engine,
            machine,
            paths,
            pal,
            recovery_counters: Default::default(),
        })
    }

    /// Returns the node's `id`.
    pub fn id(&self) -> Uuid {
        self.data.id
    }

    /// Updates OS image for VM.
    #[instrument(skip(self))]
    pub async fn upgrade(&mut self, image: &NodeImage) -> Result<()> {
        if self.status() != NodeStatus::Stopped {
            bail!("Node should be stopped before running upgrade");
        }

        self.copy_os_image(image).await?;

        self.data.image = image.clone();
        self.data.save(&self.paths.registry).await
    }

    /// Starts the node.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<()> {
        if self.status() == NodeStatus::Running {
            return Ok(());
        }
        if self.status() == NodeStatus::Failed && self.expected_status() == NodeStatus::Stopped {
            bail!("can't start node which is not stopped properly");
        }

        if self.machine.state() == firec::MachineState::SHUTOFF {
            self.data.network_interface.remaster().await?;
            self.machine.start().await?;
        }
        self.babel_engine.node_conn = Self::connect(
            &self.paths.chroot,
            self.pal.babel_path(),
            self.id(),
            NODE_START_TIMEOUT,
        )
        .await?;
        let babelsup_client = self.babel_engine.node_conn.babelsup_client().await?;
        with_retry!(babelsup_client.setup_supervisor(self.data.babel_conf.supervisor.clone()))?;
        if let Some(firewall_config) = &self.data.babel_conf.firewall {
            let babel_client = self.babel_engine.node_conn.babel_client().await?;
            with_retry!(babel_client.setup_firewall(firewall_config.clone()))?;
        }
        Ok(())
    }

    pub async fn set_expected_status(&mut self, status: NodeStatus) -> Result<()> {
        self.data.expected_status = status;
        self.data.save(&self.paths.registry).await
    }

    /// Returns the actual status of the node.
    pub fn status(&self) -> NodeStatus {
        let machine_status = match self.machine.state() {
            firec::MachineState::RUNNING => NodeStatus::Running,
            firec::MachineState::SHUTOFF => NodeStatus::Stopped,
        };
        if machine_status == self.data.expected_status {
            if machine_status == NodeStatus::Running
                && (self.babel_engine.node_conn.is_closed()
                    || self.babel_engine.node_conn.is_broken())
            {
                // node is running, but there is no babel connection or is broken for some reason
                NodeStatus::Failed
            } else {
                machine_status
            }
        } else {
            NodeStatus::Failed
        }
    }

    pub async fn recover(&mut self) -> Result<()> {
        let id = self.id();
        match self.data.expected_status {
            NodeStatus::Running => {
                if self.machine.state() == firec::MachineState::SHUTOFF {
                    self.started_node_recovery().await?;
                } else {
                    self.node_connection_recovery().await?;
                }
            }
            NodeStatus::Stopped => {
                self.recovery_counters.stop += 1;
                info!("Recovery: stopping node with ID `{id}`");
                if let Err(e) = self.stop().await {
                    warn!("Recovery: stopping node with ID `{id}` failed: {e}");
                    if self.recovery_counters.stop >= MAX_STOP_TRIES {
                        error!("Recovery: retries count exceeded, mark as failed");
                        self.set_expected_status(NodeStatus::Failed).await?;
                    }
                } else {
                    self.post_recovery();
                }
            }
            NodeStatus::Failed => {
                warn!("Recovery: node with ID `{id}` cannot be recovered");
            }
        }
        Ok(())
    }

    async fn connect(
        chroot_path: &Path,
        babel_path: &Path,
        node_id: Uuid,
        max_delay: Duration,
    ) -> Result<NodeConnection> {
        let mut connection = NodeConnection::try_open(chroot_path, node_id, max_delay).await?;
        let (babel_bin, checksum) = Self::load_babel_bin(babel_path).await?;
        let client = connection.babelsup_client().await?;
        let babel_status = with_retry!(client.check_babel(checksum))?.into_inner();
        if babel_status != babel_api::BinaryStatus::Ok {
            info!("Invalid or missing Babel service on VM, installing new one");
            with_retry!(client.start_new_babel(tokio_stream::iter(babel_bin.clone())))?;
        }
        Ok(connection)
    }

    async fn load_babel_bin(babel_path: &Path) -> Result<(Vec<babel_api::Binary>, u32)> {
        let file = File::open(babel_path)
            .await
            .with_context(|| format!("failed to load babel binary {}", babel_path.display()))?;
        let mut reader = BufReader::new(file);
        let mut buf = [0; 16384];
        let crc = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);
        let mut digest = crc.digest();
        let mut babel_bin = Vec::<babel_api::Binary>::default();
        while let Ok(size) = reader.read(&mut buf[..]).await {
            if size == 0 {
                break;
            }
            digest.update(&buf[0..size]);
            babel_bin.push(babel_api::Binary::Bin(buf[0..size].to_vec()));
        }
        let checksum = digest.finalize();
        babel_bin.push(babel_api::Binary::Checksum(checksum));
        Ok((babel_bin, checksum))
    }

    async fn started_node_recovery(&mut self) -> Result<()> {
        let id = self.id();
        self.recovery_counters.start += 1;
        info!("Recovery: starting node with ID `{id}`");
        if let Err(e) = self.start().await {
            warn!("Recovery: starting node with ID `{id}` failed: {e}");
            if self.recovery_counters.start >= MAX_START_TRIES {
                error!("Recovery: retries count exceeded, mark as failed");
                self.set_expected_status(NodeStatus::Failed).await?;
            }
        } else {
            self.post_recovery();
        }
        Ok(())
    }

    async fn node_connection_recovery(&mut self) -> Result<()> {
        let id = self.id();
        self.recovery_counters.reconnect += 1;
        info!("Recovery: fix broken connection to node with ID `{id}`");
        if let Err(e) = self.babel_engine.node_conn.connection_test().await {
            warn!("Recovery: reconnect to node with ID `{id}` failed: {e}");
            if self.recovery_counters.reconnect >= MAX_RECONNECT_TRIES {
                info!("Recovery: restart broken node with ID `{id}`");

                self.recovery_counters.stop += 1;
                if let Err(e) = self.stop().await {
                    warn!("Recovery: stopping node with ID `{id}` failed: {e}");
                    if self.recovery_counters.stop >= MAX_STOP_TRIES {
                        error!("Recovery: retries count exceeded, mark as failed");
                        self.set_expected_status(NodeStatus::Failed).await?;
                    }
                } else {
                    self.started_node_recovery().await?;
                }
            }
        } else if self.babel_engine.node_conn.is_closed() {
            // node wasn't fully started so proceed with other stuff
            self.started_node_recovery().await?;
        } else {
            self.post_recovery();
        }
        Ok(())
    }

    fn post_recovery(&mut self) {
        // reset counters on successful recovery
        self.recovery_counters = Default::default();
    }

    /// Returns the expected status of the node.
    pub fn expected_status(&self) -> NodeStatus {
        self.data.expected_status
    }

    /// Stops the running node.
    #[instrument(skip(self))]
    pub async fn stop(&mut self) -> Result<()> {
        match self.machine.state() {
            firec::MachineState::SHUTOFF => {}
            firec::MachineState::RUNNING => {
                if let Err(err) = self.machine.shutdown().await {
                    warn!("Graceful shutdown failed: {err}");

                    if let Err(err) = self.machine.force_shutdown().await {
                        bail!("Forced shutdown failed: {err}");
                    }
                }
            }
        }

        let start = Instant::now();
        loop {
            match self.machine.state() {
                firec::MachineState::RUNNING if start.elapsed() < NODE_STOP_TIMEOUT => {
                    debug!("Firecracker process not shutdown yet, will retry");
                    tokio::time::sleep(NODE_STOPPED_CHECK_INTERVAL).await;
                }
                firec::MachineState::RUNNING => {
                    bail!("Firecracker shutdown timeout");
                }
                firec::MachineState::SHUTOFF => break,
            }
        }
        self.babel_engine.node_conn = NodeConnection::closed(&self.paths.chroot, self.id());

        Ok(())
    }

    /// Deletes the node.
    #[instrument(skip(self))]
    pub async fn delete(self) -> Result<()> {
        self.machine.delete().await?;
        self.data.delete(&self.paths.registry).await
    }

    pub async fn update(
        &mut self,
        name: Option<String>,
        self_update: Option<bool>,
        properties: Vec<Parameter>,
    ) -> Result<()> {
        // If the fields we receive are populated, we update the node data.
        if let Some(name) = name {
            // TODO: we need to remove it from protos
            if self.data.name == name {
                warn!("Cannot change node name to `{name}`, operation is not supported");
            }
        }
        if let Some(self_update) = self_update {
            self.data.self_update = self_update;
        }
        if !properties.is_empty() {
            // TODO change API to send Option<Vec<Parameter>> to allow setting empty properties
            self.data.properties = properties.into_iter().map(|p| (p.name, p.value)).collect();
        }
        self.data.save(&self.paths.registry).await
    }

    pub async fn firewall_update(&mut self, config: firewall::Config) -> Result<()> {
        babel_api::check_firewall_config(&config)?;
        let babel_client = self.babel_engine.node_conn.babel_client().await?;
        with_retry!(babel_client.setup_firewall(config.clone()))?;
        self.data.babel_conf.firewall = Some(config);
        self.data.save(&self.paths.registry).await
    }

    /// Check if chroot location contains valid data
    pub async fn is_data_valid(&self) -> Result<bool> {
        let data_dir = &self.paths.data_dir;

        let root = data_dir.join(ROOT_FS_FILE);
        let kernel = data_dir.join(KERNEL_FILE);
        let data = data_dir.join(DATA_FILE);
        if !root.exists() || !kernel.exists() || !data.exists() {
            return Ok(false);
        }

        Ok(fs::metadata(root).await?.len() > 0
            && fs::metadata(kernel).await?.len() > 0
            && fs::metadata(data).await?.len() > 0)
    }

    /// Copy OS drive into chroot location.
    async fn copy_os_image(&self, image: &NodeImage) -> Result<()> {
        let root_fs_path =
            CookbookService::get_image_download_folder_path(&self.paths.bv_root, image)
                .join(ROOT_FS_FILE);

        let data_dir = &self.paths.data_dir;
        DirBuilder::new().recursive(true).create(data_dir).await?;

        run_cmd("cp", [root_fs_path.as_os_str(), data_dir.as_os_str()]).await?;

        Ok(())
    }

    /// Create new data drive in chroot location.
    async fn create_data_image(paths: &Paths, disk_size_gb: usize) -> Result<()> {
        let data_dir = &paths.data_dir;
        DirBuilder::new().recursive(true).create(data_dir).await?;
        let path = data_dir.join(DATA_FILE);

        let gb = &format!("{disk_size_gb}G");
        run_cmd(
            "fallocate",
            [OsStr::new("-l"), OsStr::new(gb), path.as_os_str()],
        )
        .await?;
        run_cmd("mkfs.ext4", [path.as_os_str()]).await?;

        Ok(())
    }

    async fn create_config(
        paths: &Paths,
        data: &NodeData<<P as Pal>::NetInterface>,
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
        let iface =
            firec::config::network::Interface::new(data.network_interface.name().clone(), "eth0");
        let root_fs_path =
            CookbookService::get_image_download_folder_path(&paths.bv_root, &data.image)
                .join(ROOT_FS_FILE);
        let kernel_path =
            CookbookService::get_image_download_folder_path(&paths.bv_root, &data.image)
                .join(KERNEL_FILE);

        let config = firec::config::Config::builder(Some(data.id), kernel_path)
            // Jailer configuration.
            .jailer_cfg()
            .chroot_base_dir(paths.chroot.clone())
            .exec_file(paths.bv_root.join(FC_BIN_PATH))
            .mode(JailerMode::Tmux(Some(data.name.clone().into())))
            .build()
            // Machine configuration.
            .machine_cfg()
            .vcpu_count(data.babel_conf.requirements.vcpu_count)
            .mem_size_mib(data.babel_conf.requirements.mem_size_mb as i64)
            .build()
            // Add root drive.
            .add_drive("root", root_fs_path)
            .is_root_device(true)
            .build()
            // Add data drive.
            .add_drive("data", paths.data.clone())
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
}
