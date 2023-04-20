use crate::{
    babel_engine,
    node_connection::{BabelConnection, NodeConnection},
    node_data::{NodeData, NodeImage, NodeStatus},
    pal::{NetInterface, Pal},
    services::cookbook::{CookbookService, BABEL_PLUGIN_NAME},
    utils::get_process_pid,
    with_retry, BV_VAR_PATH,
};
use anyhow::{bail, Context, Result};
use babel_api::{
    babelsup::SupervisorConfig,
    metadata::{firewall, BlockchainMetadata},
    rhai_plugin,
    rhai_plugin::RhaiPlugin,
};
use bv_utils::cmd::run_cmd;
use chrono::{DateTime, Utc};
use firec::{config::JailerMode, Machine};
use std::{
    ffi::OsStr,
    fmt::Debug,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs::{self, DirBuilder, File},
    io::{AsyncReadExt, BufReader},
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
const SUPERVISOR_CONFIG: SupervisorConfig = SupervisorConfig {
    backoff_timeout_ms: 3000,
    backoff_base_ms: 200,
};

pub fn build_registry_dir(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_DIR)
}

pub type BabelEngine = babel_engine::BabelEngine<NodeConnection, RhaiPlugin<babel_engine::Engine>>;

#[derive(Debug)]
pub struct Node<P: Pal> {
    pub data: NodeData<<P as Pal>::NetInterface>,
    pub babel_engine: BabelEngine,
    metadata: BlockchainMetadata,
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
    data: PathBuf,
    plugin_data: PathBuf,
    registry: PathBuf,
}

impl Paths {
    fn build(bv_root: &Path, id: Uuid) -> Self {
        let registry = build_registry_dir(bv_root);
        Self {
            bv_root: bv_root.to_path_buf(),
            chroot: bv_root.join(BV_VAR_PATH),
            data_dir: bv_root
                .join(BV_VAR_PATH)
                .join(FC_BIN_NAME)
                .join(id.to_string())
                .join("root"),
            data: bv_root.join(BV_VAR_PATH).join(DATA_FILE),
            plugin_data: registry.join(format!("{id}.data")),
            registry,
        }
    }

    fn script_file_path(registry_config_dir: &Path, id: Uuid) -> PathBuf {
        registry_config_dir.join(format!("{id}.rhai"))
    }
}

impl<P: Pal + Debug> Node<P> {
    /// Creates a new node according to specs.
    #[instrument(skip(data))]
    pub async fn create(pal: Arc<P>, data: NodeData<<P as Pal>::NetInterface>) -> Result<Self> {
        info!("Creating node with data: {data:?}");
        let node_id = data.id;
        let paths = Paths::build(pal.bv_root(), node_id);

        let (script, metadata) = Self::copy_and_check_plugin(&paths, node_id, &data.image).await?;

        let _ = tokio::fs::remove_dir_all(&paths.data_dir).await;
        let config = Self::create_config(&paths, &data).await?;
        Self::create_data_image(&paths, data.requirements.disk_size_gb).await?;
        let machine = Machine::create(config).await?;

        data.save(&paths.registry).await?;

        let babel_engine = BabelEngine::new(
            node_id,
            NodeConnection::closed(&paths.chroot, node_id),
            |engine| RhaiPlugin::new(&script, engine),
            data.properties.clone(),
            paths.plugin_data.clone(),
        )?;
        Ok(Self {
            data,
            babel_engine,
            metadata,
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
        let paths = Paths::build(pal.bv_root(), data.id);

        let script = fs::read_to_string(&Paths::script_file_path(&paths.registry, data.id)).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;

        let config = Self::create_config(&paths, &data).await?;
        let node_id = data.id;
        let cmd = node_id.to_string();
        let (pid, node_conn) = match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(pid) => {
                // Since this is the startup phase it doesn't make sense to wait a long time
                // for the nodes to come online. For that reason we restrict the allowed delay
                // further down.
                debug!("connecting to babel ...");
                let node_conn = match Self::connect(
                    &paths.chroot,
                    pal.babel_path(),
                    node_id,
                    NODE_RECONNECT_TIMEOUT,
                )
                .await
                {
                    Ok(mut node_conn) => {
                        if let Err(err) =
                            Self::check_job_runner(&mut node_conn, pal.job_runner_path()).await
                        {
                            warn!("failed to check/update job runner on running node {node_id}: {err}");
                            NodeConnection::closed(&paths.chroot, node_id)
                        } else {
                            node_conn
                        }
                    }
                    Err(err) => {
                        warn!("failed to reestablish babel connection to running node {node_id}: {err}");
                        NodeConnection::closed(&paths.chroot, node_id)
                    }
                };
                (Some(pid), node_conn)
            }
            Err(_) => (None, NodeConnection::closed(&paths.chroot, node_id)),
        };
        let machine = Machine::connect(config, pid).await;
        let babel_engine = BabelEngine::new(
            node_id,
            node_conn,
            |engine| RhaiPlugin::new(&script, engine),
            data.properties.clone(),
            paths.plugin_data.clone(),
        )?;
        Ok(Self {
            data,
            babel_engine,
            metadata,
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

        let (script, metadata) =
            Self::copy_and_check_plugin(&self.paths, self.data.id, &self.data.image).await?;
        self.metadata = metadata;
        self.babel_engine
            .update_plugin(|engine| RhaiPlugin::new(&script, engine))?;

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
        let id = self.id();
        self.babel_engine.babel_connection = Self::connect(
            &self.paths.chroot,
            self.pal.babel_path(),
            id,
            NODE_START_TIMEOUT,
        )
        .await?;
        let babelsup_client = self.babel_engine.babel_connection.babelsup_client().await?;
        with_retry!(babelsup_client.setup_supervisor(SUPERVISOR_CONFIG))?;
        Self::check_job_runner(
            &mut self.babel_engine.babel_connection,
            self.pal.job_runner_path(),
        )
        .await?;

        // setup babel
        let babel_client = self.babel_engine.babel_connection.babel_client().await?;
        match babel_client
            .setup_babel((id.to_string(), self.metadata.babel_config.clone()))
            .await
        {
            Ok(_) => {}
            Err(e) if e.code() == tonic::Code::AlreadyExists => {}
            Err(e) => bail!(e),
        }

        // setup firewall
        let mut firewall_config = self.metadata.firewall.clone();
        firewall_config
            .rules
            .append(&mut self.data.firewall_rules.clone());
        with_retry!(babel_client.setup_firewall(firewall_config.clone()))?;

        Ok(())
    }

    pub async fn set_expected_status(&mut self, status: NodeStatus) -> Result<()> {
        self.data.expected_status = status;
        self.data.save(&self.paths.registry).await
    }

    pub async fn set_started_at(&mut self, started_at: Option<DateTime<Utc>>) -> Result<()> {
        self.data.started_at = started_at;
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
                && (self.babel_engine.babel_connection.is_closed()
                    || self.babel_engine.babel_connection.is_broken())
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
        // check and update babel
        let (babel_bin, checksum) = Self::load_bin(babel_path).await?;
        let client = connection.babelsup_client().await?;
        let babel_status = with_retry!(client.check_babel(checksum))?.into_inner();
        if babel_status != babel_api::utils::BinaryStatus::Ok {
            info!("Invalid or missing Babel service on VM, installing new one");
            with_retry!(client.start_new_babel(tokio_stream::iter(babel_bin.clone())))?;
        }
        Ok(connection)
    }

    async fn check_job_runner(
        connection: &mut NodeConnection,
        job_runner_path: &Path,
    ) -> Result<()> {
        // check and update job_runner
        let (babel_bin, checksum) = Self::load_bin(job_runner_path).await?;
        let client = connection.babel_client().await?;
        let job_runner_status = with_retry!(client.check_job_runner(checksum))?.into_inner();
        if job_runner_status != babel_api::utils::BinaryStatus::Ok {
            info!("Invalid or missing JobRunner service on VM, installing new one");
            with_retry!(client.upload_job_runner(tokio_stream::iter(babel_bin.clone())))?;
        }
        Ok(())
    }

    async fn load_bin(bin_path: &Path) -> Result<(Vec<babel_api::utils::Binary>, u32)> {
        let file = File::open(bin_path)
            .await
            .with_context(|| format!("failed to load binary {}", bin_path.display()))?;
        let mut reader = BufReader::new(file);
        let mut buf = [0; 16384];
        let crc = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);
        let mut digest = crc.digest();
        let mut babel_bin = Vec::<babel_api::utils::Binary>::default();
        while let Ok(size) = reader.read(&mut buf[..]).await {
            if size == 0 {
                break;
            }
            digest.update(&buf[0..size]);
            babel_bin.push(babel_api::utils::Binary::Bin(buf[0..size].to_vec()));
        }
        let checksum = digest.finalize();
        babel_bin.push(babel_api::utils::Binary::Checksum(checksum));
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
        if let Err(e) = self.babel_engine.babel_connection.connection_test().await {
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
        } else if self.babel_engine.babel_connection.is_closed() {
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
        self.babel_engine.babel_connection = NodeConnection::closed(&self.paths.chroot, self.id());

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
        self_update: Option<bool>,
        rules: Vec<firewall::Rule>,
    ) -> Result<()> {
        // If the fields we receive are populated, we update the node data.
        if let Some(self_update) = self_update {
            self.data.self_update = self_update;
        }
        babel_api::metadata::check_firewall_rules(&rules)?;
        let mut firewall = self.metadata.firewall.clone();
        firewall.rules.append(&mut rules.clone());
        let babel_client = self.babel_engine.babel_connection.babel_client().await?;
        with_retry!(babel_client.setup_firewall(firewall.clone()))?;
        self.data.firewall_rules = rules;
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

        let gb = &format!("{disk_size_gb}GB");
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
            .vcpu_count(data.requirements.vcpu_count)
            .mem_size_mib(data.requirements.mem_size_mb as i64)
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

    /// copy plugin script into nodes registry and read metadata form it
    async fn copy_and_check_plugin(
        paths: &Paths,
        id: Uuid,
        image: &NodeImage,
    ) -> Result<(String, BlockchainMetadata)> {
        let script_path = Paths::script_file_path(&paths.registry, id);
        fs::copy(
            CookbookService::get_image_download_folder_path(&paths.bv_root, image)
                .join(BABEL_PLUGIN_NAME),
            &script_path,
        )
        .await?;
        let script = fs::read_to_string(&script_path).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;
        Ok((script, metadata))
    }
}
