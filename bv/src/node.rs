use anyhow::{anyhow, bail, Context, Result};
use babel_api::config::KeysConfig;
use babel_api::config::Method::{Jrpc, Rest, Sh};
use firec::config::JailerMode;
use firec::Machine;
use futures_util::StreamExt;
use std::collections::hash_map::Entry;
use std::ffi::OsStr;
use std::fmt::{Debug, Display};
use std::path::PathBuf;
use std::sync::Arc;
use std::{collections::HashMap, path::Path, str::FromStr, time::Duration};
use tokio::fs::DirBuilder;
use tokio::time::Instant;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::pal::{NetInterface, Pal};
use crate::services::api::pb::Parameter;
use crate::{
    node_connection::NodeConnection,
    node_data::{NodeData, NodeImage, NodeStatus},
    render,
    services::cookbook::CookbookService,
    utils::{get_process_pid, run_cmd},
    with_retry, BV_VAR_PATH,
};

const NODE_START_TIMEOUT: Duration = Duration::from_secs(60);
const NODE_RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const NODE_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const NODE_STOPPED_CHECK_INTERVAL: Duration = Duration::from_secs(1);
pub const REGISTRY_CONFIG_DIR: &str = "nodes";
pub const FC_BIN_NAME: &str = "firecracker";
const DATA_IMG_FILENAME: &str = "data.img";
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
const MAX_RECONNECT_TRIES: usize = 5;

pub fn build_registry_dir(bv_root: &Path) -> PathBuf {
    bv_root.join(BV_VAR_PATH).join(REGISTRY_CONFIG_DIR)
}

#[derive(Debug)]
pub struct Node<P: Pal> {
    pub data: NodeData<<P as Pal>::NetInterface>,
    machine: Machine<'static>,
    node_conn: NodeConnection,
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
    registry: PathBuf,
    data: PathBuf,
}

impl Paths {
    fn build(bv_root: &Path) -> Self {
        Self {
            bv_root: bv_root.to_path_buf(),
            chroot: bv_root.join(BV_VAR_PATH),
            registry: build_registry_dir(bv_root),
            data: bv_root.join(BV_VAR_PATH).join(DATA_IMG_FILENAME),
        }
    }
}

impl<P: Pal + Debug> Node<P> {
    /// Creates a new node according to specs.
    #[instrument(skip(data))]
    pub async fn create(pal: Arc<P>, data: NodeData<<P as Pal>::NetInterface>) -> Result<Node<P>> {
        info!("Creating node with data: {data:?}");
        let node_id = data.id;
        let paths = Paths::build(pal.bv_root());
        let config = Node::<P>::create_config(&paths, &data).await?;
        Node::<P>::create_data_image(&paths, &node_id, data.babel_conf.requirements.disk_size_gb)
            .await?;
        let machine = Machine::create(config).await?;

        data.save(&paths.registry).await?;

        Ok(Self {
            data,
            machine,
            node_conn: NodeConnection::closed(&paths.chroot, node_id),
            paths,
            pal,
            recovery_counters: Default::default(),
        })
    }

    /// Returns node previously created on this host.
    #[instrument(skip(data))]
    pub async fn attach(pal: Arc<P>, data: NodeData<<P as Pal>::NetInterface>) -> Result<Node<P>> {
        info!("Attaching to node with data: {data:?}");
        let paths = Paths::build(pal.bv_root());
        let config = Node::<P>::create_config(&paths, &data).await?;
        let cmd = data.id.to_string();
        let (pid, node_conn) = match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(pid) => {
                // Since this is the startup phase it doesn't make sense to wait a long time
                // for the nodes to come online. For that reason we restrict the allowed delay
                // further down.
                debug!("connecting to babel ...");
                let node_conn = NodeConnection::try_open(
                    &paths.chroot,
                    pal.babel_path(),
                    data.id,
                    NODE_RECONNECT_TIMEOUT,
                )
                .await
                .unwrap_or_else(|err| {
                    warn!(
                        "failed to reestablished babel connection to running node {}: {}",
                        data.id, err
                    );
                    NodeConnection::closed(&paths.chroot, data.id)
                });
                (Some(pid), node_conn)
            }
            Err(_) => (None, NodeConnection::closed(&paths.chroot, data.id)),
        };
        let machine = Machine::connect(config, pid).await;
        Ok(Self {
            data,
            machine,
            node_conn,
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
        self.node_conn = NodeConnection::try_open(
            &self.paths.chroot,
            self.pal.babel_path(),
            self.id(),
            NODE_START_TIMEOUT,
        )
        .await?;
        let babelsup_client = self.node_conn.babelsup_client().await?;
        with_retry!(babelsup_client.setup_supervisor(self.data.babel_conf.supervisor.clone()))?;
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
                && (self.node_conn.is_closed() || self.node_conn.is_broken())
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
                if self.machine.state() == firec::MachineState::SHUTOFF
                    || self.node_conn.is_closed()
                {
                    self.started_node_recovery().await?;
                } else if self.node_conn.is_broken() {
                    self.node_connection_recovery().await?;
                }
            }
            NodeStatus::Stopped => {
                self.recovery_counters.stop += 1;
                info!("Recovery: stopping node with ID `{id}`");
                if let Err(e) = self.stop().await {
                    error!("Recovery: stopping node with ID `{id}` failed: {e}");
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

    async fn started_node_recovery(&mut self) -> Result<()> {
        self.recovery_counters.start += 1;
        if let Err(e) = self.start().await {
            let id = self.id();
            error!("Recovery: starting node with ID `{id}` failed: {e}");
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
        if let Err(e) = self.babelsup_connection_test().await {
            error!("Recovery: reconnect to node with ID `{id}` failed: {e}");
            if self.recovery_counters.reconnect >= MAX_RECONNECT_TRIES {
                error!("Recovery: restart broken node with ID `{id}`");

                self.recovery_counters.stop += 1;
                if let Err(e) = self.stop().await {
                    error!("Recovery: stopping node with ID `{id}` failed: {e}");
                    if self.recovery_counters.stop >= MAX_STOP_TRIES {
                        error!("Recovery: retries count exceeded, mark as failed");
                        self.set_expected_status(NodeStatus::Failed).await?;
                    }
                } else {
                    self.started_node_recovery().await?;
                }
            }
        } else {
            self.post_recovery();
        }
        Ok(())
    }

    fn post_recovery(&mut self) {
        // reset counters on successful recovery
        self.recovery_counters = Default::default();
    }

    async fn babelsup_connection_test(&mut self) -> Result<()> {
        let client = self.node_conn.babelsup_client().await?;
        with_retry!(client.get_version(()))?;
        Ok(())
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
        let elapsed = || Instant::now() - start;
        loop {
            match self.machine.state() {
                firec::MachineState::RUNNING if elapsed() < NODE_STOP_TIMEOUT => {
                    debug!("Firecracker process not shutdown yet, will retry");
                    tokio::time::sleep(NODE_STOPPED_CHECK_INTERVAL).await;
                }
                firec::MachineState::RUNNING => {
                    bail!("Firecracker shutdown timeout");
                }
                firec::MachineState::SHUTOFF => break,
            }
        }
        self.node_conn = NodeConnection::closed(&self.paths.chroot, self.id());

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
            self.data.name = name;
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

    /// Copy OS drive into chroot location.
    async fn copy_os_image(&self, image: &NodeImage) -> Result<()> {
        let root_fs_path =
            CookbookService::get_image_download_folder_path(&self.paths.bv_root, image)
                .join(ROOT_FS_FILE);

        let data_dir = self
            .paths
            .chroot
            .join(FC_BIN_NAME)
            .join(self.id().to_string())
            .join("root");
        DirBuilder::new().recursive(true).create(&data_dir).await?;

        run_cmd("cp", [root_fs_path.as_os_str(), data_dir.as_os_str()]).await?;

        Ok(())
    }

    /// Create new data drive in chroot location.
    async fn create_data_image(paths: &Paths, id: &Uuid, disk_size_gb: usize) -> Result<()> {
        let data_dir = paths
            .chroot
            .join(FC_BIN_NAME)
            .join(id.to_string())
            .join("root");
        DirBuilder::new().recursive(true).create(&data_dir).await?;
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
            bail!("to long kernel_args {kernel_args}")
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

    /// Returns the height of the blockchain (in blocks).
    pub async fn height(&mut self) -> Result<u64> {
        self.call_method(babel_api::BabelMethod::Height, HashMap::new())
            .await
    }

    /// Returns the block age of the blockchain (in seconds).
    pub async fn block_age(&mut self) -> Result<u64> {
        self.call_method(babel_api::BabelMethod::BlockAge, HashMap::new())
            .await
    }

    /// Returns the name of the node. This is usually some random generated name that you may use
    /// to recognise the node, but the purpose may vary per blockchain.
    /// ### Example
    /// `chilly-peach-kangaroo`
    pub async fn name(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::Name, HashMap::new())
            .await
    }

    /// The address of the node. The meaning of this varies from blockchain to blockchain.
    /// ### Example
    /// `/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3`
    pub async fn address(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::Address, HashMap::new())
            .await
    }

    /// Returns whether this node is in consensus or not.
    pub async fn consensus(&mut self) -> Result<bool> {
        self.call_method(babel_api::BabelMethod::Consensus, HashMap::new())
            .await
    }

    pub async fn application_status(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::ApplicationStatus, HashMap::new())
            .await
    }

    pub async fn sync_status(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::SyncStatus, HashMap::new())
            .await
    }

    pub async fn staking_status(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::StakingStatus, HashMap::new())
            .await
    }

    pub async fn init(&mut self, secret_keys: HashMap<String, Vec<u8>>) -> Result<String> {
        let mut node_keys = self
            .data
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), vec![v.clone()]))
            .collect::<HashMap<_, _>>();

        for (k, v) in secret_keys.into_iter() {
            match node_keys.entry(k) {
                Entry::Occupied(entry) => {
                    // Parameter KEY that comes form backend or possibly from user can not be the same
                    // as KEY from node_keys map. That could lead to undefined behaviour or even
                    // security issue. User provided value could be (unintentionally) joined with
                    // node_keys value into a single parameter and used in not desirable place.
                    // Since user has no way to define KEYs it must be treated as internal error
                    // (that shall never happen, but ...).
                    bail!("Secret keys KEY collides with params KEY: {}", entry.key())
                }
                Entry::Vacant(entry) => {
                    entry.insert(vec![String::from_utf8(v)?]);
                }
            }
        }

        self.call_method(babel_api::BabelMethod::Init, node_keys)
            .await
    }

    /// This function calls babel by sending a blockchain command using the specified method name.
    #[instrument(skip(self), fields(id = %self.data.id, name = name.to_string()), err, ret(Debug))]
    pub async fn call_method<T>(
        &mut self,
        name: impl Display,
        params: HashMap<String, Vec<String>>,
    ) -> Result<T>
    where
        T: FromStr + Debug,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        let get_api_host = |method: &str| -> Result<&String> {
            self.data
                .babel_conf
                .config
                .api_host
                .as_ref()
                .ok_or_else(|| anyhow!("No host specified for method `{method}"))
        };
        let join_params = |v: &Vec<String>| Ok(v.join(","));
        let conf = toml::Value::try_from(&self.data.babel_conf)?;
        let babel_client = self.node_conn.babel_client().await?;
        let resp = match self
            .data
            .babel_conf
            .methods
            .get(&name.to_string())
            .ok_or_else(|| anyhow!("method `{name}` not found"))?
        {
            Jrpc {
                method, response, ..
            } => {
                with_retry!(babel_client.blockchain_jrpc((
                    get_api_host(method)?.clone(),
                    render::render_with(method, &params, &conf, join_params)?,
                    response.clone(),
                )))
            }
            Rest {
                method, response, ..
            } => {
                let host = get_api_host(method)?;

                let url = format!(
                    "{}/{}",
                    host.trim_end_matches('/'),
                    method.trim_start_matches('/')
                );

                with_retry!(babel_client.blockchain_rest((
                    render::render_with(&url, &params, &conf, join_params)?,
                    response.clone(),
                )))
            }
            Sh { body, response, .. } => {
                with_retry!(babel_client.blockchain_sh((
                    render::render_with(body, &params, &conf, render::render_sh_param)?,
                    response.clone(),
                )))
            }
        };
        let resp = match resp
            .map_err(|err| {
                self.node_conn.mark_broken();
                err
            })?
            .into_inner()
        {
            Ok(value) => value
                .parse()
                .context(format!("Could not parse {name} response: {value}"))?,
            Err(err) => bail!(err),
        };
        Ok(resp)
    }

    /// Returns the methods that are supported by this blockchain. Calling any method on this
    /// blockchain that is not listed here will result in an error being returned.
    pub fn capabilities(&mut self) -> Vec<String> {
        self.data
            .babel_conf
            .methods
            .keys()
            .map(|method| method.to_string())
            .collect()
    }

    /// Checks if node has some particular capability
    pub fn has_capability(&mut self, method: &str) -> bool {
        self.capabilities().contains(&method.to_owned())
    }

    /// Returns the list of logs from blockchain entry_points.
    pub async fn get_logs(&mut self) -> Result<Vec<String>> {
        let client = self.node_conn.babelsup_client().await?;
        let mut resp = with_retry!(client.get_logs(()))?.into_inner();
        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = resp.next().await {
            logs.push(log);
        }
        Ok(logs)
    }

    /// Returns blockchain node keys.
    pub async fn download_keys(&mut self) -> Result<Vec<babel_api::BlockchainKey>> {
        let config = self.get_keys_config()?;
        let babel_client = self.node_conn.babel_client().await?;
        let keys = with_retry!(babel_client.download_keys(config.clone()))?.into_inner();
        Ok(keys)
    }

    /// Sets blockchain node keys.
    pub async fn upload_keys(&mut self, keys: Vec<babel_api::BlockchainKey>) -> Result<()> {
        let config = self.get_keys_config()?;
        let babel_client = self.node_conn.babel_client().await?;
        with_retry!(babel_client.upload_keys((config.clone(), keys.clone())))?;
        Ok(())
    }

    fn get_keys_config(&mut self) -> Result<KeysConfig> {
        self.data
            .babel_conf
            .keys
            .clone()
            .ok_or_else(|| anyhow!("No `keys` section found in config"))
    }

    /// Generates keys on node
    pub async fn generate_keys(&mut self) -> Result<String> {
        self.call_method(babel_api::BabelMethod::GenerateKeys, HashMap::new())
            .await
    }
}
