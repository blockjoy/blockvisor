use crate::{
    babel_engine,
    babel_engine::NodeInfo,
    config::SharedConfig,
    node_data::{NodeData, NodeImage, NodeStatus},
    pal,
    pal::NodeConnection,
    pal::VirtualMachine,
    pal::{NetInterface, Pal},
    services::{
        cookbook::{CookbookService, BABEL_PLUGIN_NAME, DATA_FILE, ROOT_FS_FILE},
        kernel::KERNEL_FILE,
    },
    utils::with_timeout,
    BV_VAR_PATH,
};
use anyhow::{bail, Context, Result};
use babel_api::{
    babelsup::SupervisorConfig,
    metadata::{firewall, BlockchainMetadata},
    rhai_plugin,
    rhai_plugin::RhaiPlugin,
};
use bv_utils::{cmd::run_cmd, with_retry};
use chrono::{DateTime, Utc};
use std::{
    ffi::OsStr,
    fmt::Debug,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, BufReader},
    time::Instant,
};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

const NODE_START_TIMEOUT: Duration = Duration::from_secs(120);
const NODE_RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const NODE_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const NODE_STOPPED_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const FW_SETUP_TIMEOUT_SEC: u64 = 30;
const FW_RULE_SETUP_TIMEOUT_SEC: u64 = 1;
pub const REGISTRY_CONFIG_DIR: &str = "nodes";
pub const DATA_CACHE_DIR: &str = "blocks";
const DATA_CACHE_EXPIRATION: Duration = Duration::from_secs(7 * 24 * 3600);
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

pub type BabelEngine<N> = babel_engine::BabelEngine<N, RhaiPlugin<babel_engine::Engine>>;

#[derive(Debug)]
pub struct Node<P: Pal> {
    pub data: NodeData<P::NetInterface>,
    pub babel_engine: BabelEngine<P::NodeConnection>,
    metadata: BlockchainMetadata,
    machine: P::VirtualMachine,
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
    data_cache_dir: PathBuf,
    data_dir: PathBuf,
    plugin_data: PathBuf,
    plugin_script: PathBuf,
    registry: PathBuf,
}

impl Paths {
    fn build(pal: &impl Pal, id: Uuid) -> Self {
        let bv_root = pal.bv_root();
        let registry = build_registry_dir(bv_root);
        Self {
            bv_root: bv_root.to_path_buf(),
            data_cache_dir: bv_root.join(BV_VAR_PATH).join(DATA_CACHE_DIR),
            data_dir: pal.build_vm_data_path(id),
            plugin_data: registry.join(format!("{id}.data")),
            plugin_script: registry.join(format!("{id}.rhai")),
            registry,
        }
    }
}

impl<P: Pal + Debug> Node<P> {
    /// Creates a new node according to specs.
    #[instrument(skip(pal, api_config))]
    pub async fn create(
        pal: Arc<P>,
        api_config: SharedConfig,
        data: NodeData<P::NetInterface>,
    ) -> Result<Self> {
        let node_id = data.id;
        let paths = Paths::build(pal.as_ref(), node_id);
        info!("Creating node with ID: {node_id}");

        let (script, metadata) = Self::copy_and_check_plugin(&paths, &data.image).await?;

        let _ = tokio::fs::remove_dir_all(&paths.data_dir).await;
        Self::prepare_data_image(&paths, &data).await?;
        let machine = pal.create_vm(&data).await?;

        data.save(&paths.registry).await?;

        let babel_engine = BabelEngine::new(
            NodeInfo {
                node_id,
                image: data.image.clone(),
                properties: data.properties.clone(),
                network: data.network.clone(),
            },
            pal.create_node_connection(node_id),
            api_config,
            |engine| RhaiPlugin::new(&script, engine),
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
    #[instrument(skip(pal, api_config))]
    pub async fn attach(
        pal: Arc<P>,
        api_config: SharedConfig,
        data: NodeData<P::NetInterface>,
    ) -> Result<Self> {
        let node_id = data.id;
        let paths = Paths::build(pal.as_ref(), node_id);
        info!("Attaching to node with ID: {node_id}");

        let script = fs::read_to_string(&paths.plugin_script).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;

        let mut node_conn = pal.create_node_connection(node_id);
        let machine = pal.attach_vm(&data).await?;
        if machine.state() == pal::VmState::RUNNING {
            debug!("connecting to babel ...");
            // Since this is the startup phase it doesn't make sense to wait a long time
            // for the nodes to come online. For that reason we restrict the allowed delay
            // further down.
            if let Err(err) =
                Self::connect(&mut node_conn, NODE_RECONNECT_TIMEOUT, pal.babel_path()).await
            {
                warn!("failed to reestablish babel connection to running node {node_id}: {err}");
            } else if let Err(err) =
                Self::check_job_runner(&mut node_conn, pal.job_runner_path()).await
            {
                warn!("failed to check/update job runner on running node {node_id}: {err}");
                node_conn.close();
            }
        }
        let babel_engine = BabelEngine::new(
            NodeInfo {
                node_id,
                image: data.image.clone(),
                properties: data.properties.clone(),
                network: data.network.clone(),
            },
            node_conn,
            api_config,
            |engine| RhaiPlugin::new(&script, engine),
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

        let (script, metadata) = Self::copy_and_check_plugin(&self.paths, &self.data.image).await?;
        self.metadata = metadata;
        self.babel_engine
            .update_plugin(|engine| RhaiPlugin::new(&script, engine))?;

        self.data.image = image.clone();
        self.data.save(&self.paths.registry).await
    }

    /// Read script content and update plugin with metadata
    pub async fn reload_plugin(&mut self) -> Result<()> {
        let script = fs::read_to_string(&self.paths.plugin_script).await?;
        self.metadata = rhai_plugin::read_metadata(&script)?;
        self.babel_engine
            .update_plugin(|engine| RhaiPlugin::new(&script, engine))
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

        if self.machine.state() == pal::VmState::SHUTOFF {
            self.data.network_interface.remaster().await?;
            self.machine.start().await?;
        }
        let id = self.id();
        Self::connect(
            &mut self.babel_engine.node_connection,
            NODE_START_TIMEOUT,
            self.pal.babel_path(),
        )
        .await?;
        let babelsup_client = self.babel_engine.node_connection.babelsup_client().await?;
        with_retry!(babelsup_client.setup_supervisor(SUPERVISOR_CONFIG))?;
        Self::check_job_runner(
            &mut self.babel_engine.node_connection,
            self.pal.job_runner_path(),
        )
        .await?;

        // setup babel
        let babel_client = self.babel_engine.node_connection.babel_client().await?;
        match babel_client
            .setup_babel((id.to_string(), self.metadata.babel_config.clone()))
            .await
        {
            Ok(_) => {}
            Err(e) if e.code() == tonic::Code::AlreadyExists => {}
            Err(e) => bail!(e),
        }

        if !self.data.initialized {
            // setup firewall, but only once
            let mut firewall_config = self.metadata.firewall.clone();
            firewall_config
                .rules
                .append(&mut self.data.firewall_rules.clone());
            with_retry!(babel_client.setup_firewall(with_timeout(
                firewall_config.clone(),
                fw_setup_timeout(&firewall_config)
            )))?;
        }

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
            pal::VmState::RUNNING => NodeStatus::Running,
            pal::VmState::SHUTOFF => NodeStatus::Stopped,
        };
        if machine_status == self.data.expected_status {
            if machine_status == NodeStatus::Running
                && (self.babel_engine.node_connection.is_closed()
                    || self.babel_engine.node_connection.is_broken())
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
                if self.machine.state() == pal::VmState::SHUTOFF {
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
            NodeStatus::Busy => unreachable!(),
        }
        Ok(())
    }

    async fn connect(
        connection: &mut impl NodeConnection,
        max_delay: Duration,
        babel_path: &Path,
    ) -> Result<()> {
        connection.open(max_delay).await?;
        // check and update babel
        let (babel_bin, checksum) = Self::load_bin(babel_path).await?;
        let client = connection.babelsup_client().await?;
        let babel_status = with_retry!(client.check_babel(checksum))?.into_inner();
        if babel_status != babel_api::utils::BinaryStatus::Ok {
            info!("Invalid or missing Babel service on VM, installing new one");
            with_retry!(client.start_new_babel(tokio_stream::iter(babel_bin.clone())))?;
        }
        Ok(())
    }

    async fn check_job_runner(
        connection: &mut impl NodeConnection,
        job_runner_path: &Path,
    ) -> Result<()> {
        // check and update job_runner
        let (job_runner_bin, checksum) = Self::load_bin(job_runner_path).await?;
        let client = connection.babel_client().await?;
        let job_runner_status = with_retry!(client.check_job_runner(checksum))?.into_inner();
        if job_runner_status != babel_api::utils::BinaryStatus::Ok {
            info!("Invalid or missing JobRunner service on VM, installing new one");
            with_retry!(client.upload_job_runner(tokio_stream::iter(job_runner_bin.clone())))?;
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
        if let Err(e) = self.babel_engine.node_connection.test().await {
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
        } else if self.babel_engine.node_connection.is_closed() {
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
            pal::VmState::SHUTOFF => {}
            pal::VmState::RUNNING => {
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
                pal::VmState::RUNNING if start.elapsed() < NODE_STOP_TIMEOUT => {
                    debug!("VM not shutdown yet, will retry");
                    tokio::time::sleep(NODE_STOPPED_CHECK_INTERVAL).await;
                }
                pal::VmState::RUNNING => {
                    bail!("VM shutdown timeout");
                }
                pal::VmState::SHUTOFF => break,
            }
        }
        self.babel_engine.node_connection.close();

        Ok(())
    }

    /// Deletes the node.
    #[instrument(skip(self))]
    pub async fn delete(self) -> Result<()> {
        self.machine.delete().await?;
        let _ = fs::remove_file(&self.paths.plugin_script).await;
        let _ = fs::remove_file(&self.paths.plugin_data).await;
        self.data.delete(&self.paths.registry).await
    }

    pub async fn update(&mut self, rules: Vec<firewall::Rule>) -> Result<()> {
        let mut firewall = self.metadata.firewall.clone();
        firewall.rules.append(&mut rules.clone());
        let babel_client = self.babel_engine.node_connection.babel_client().await?;
        with_retry!(babel_client
            .setup_firewall(with_timeout(firewall.clone(), fw_setup_timeout(&firewall))))?;
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
        fs::create_dir_all(data_dir).await?;

        run_cmd("cp", [root_fs_path.as_os_str(), data_dir.as_os_str()]).await?;

        Ok(())
    }

    /// Create new data drive in chroot location, or copy it from cache
    async fn prepare_data_image(paths: &Paths, data: &NodeData<P::NetInterface>) -> Result<()> {
        let data_dir = &paths.data_dir;
        fs::create_dir_all(data_dir).await?;
        let path = data_dir.join(DATA_FILE);
        let data_cache_path = paths
            .data_cache_dir
            .join(&data.image.protocol)
            .join(&data.image.node_type)
            .join(&data.network)
            .join(DATA_FILE);
        let disk_size_gb = data.requirements.disk_size_gb;

        // check local cache
        if data_cache_path.exists() {
            let elapsed = data_cache_path.metadata()?.created()?.elapsed()?;
            if elapsed < DATA_CACHE_EXPIRATION {
                run_cmd("cp", [data_cache_path.as_os_str(), path.as_os_str()]).await?;
                // TODO: ask Sean how to better resize images
                // in case cached image size is different from disk_size_gb
            } else {
                // clean up expired cache data
                fs::remove_file(data_cache_path).await?;
                // TODO: use cookbook to download new image
            }
        }

        // allocate new image on location, if it's not there yet
        if !path.exists() {
            let gb = &format!("{disk_size_gb}GB");
            run_cmd(
                "fallocate",
                [OsStr::new("-l"), OsStr::new(gb), path.as_os_str()],
            )
            .await?;
            run_cmd("mkfs.ext4", [path.as_os_str()]).await?;
        }

        Ok(())
    }

    /// copy plugin script into nodes registry and read metadata form it
    async fn copy_and_check_plugin(
        paths: &Paths,
        image: &NodeImage,
    ) -> Result<(String, BlockchainMetadata)> {
        fs::copy(
            CookbookService::get_image_download_folder_path(&paths.bv_root, image)
                .join(BABEL_PLUGIN_NAME),
            &paths.plugin_script,
        )
        .await?;
        let script = fs::read_to_string(&paths.plugin_script).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;
        Ok((script, metadata))
    }
}

fn fw_setup_timeout(config: &firewall::Config) -> Duration {
    Duration::from_secs(
        FW_SETUP_TIMEOUT_SEC + FW_RULE_SETUP_TIMEOUT_SEC * config.rules.len() as u64,
    )
}
