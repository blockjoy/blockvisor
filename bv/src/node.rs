use anyhow::{anyhow, bail, Context, Result};
use babel_api::config::KeysConfig;
use babel_api::config::Method::{Jrpc, Rest, Sh};
use babel_api::{BabelRequest, BabelResponse};
use firec::config::JailerMode;
use firec::Machine;
use futures_util::StreamExt;
use std::ffi::OsStr;
use std::fmt::Display;
use std::{collections::HashMap, path::Path, str::FromStr, time::Duration};
use tokio::{fs::DirBuilder, time::sleep};
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::{
    env::*,
    node_connection::NodeConnection,
    node_data::{NodeData, NodeImage, NodeStatus},
    services::cookbook::CookbookService,
    utils,
    utils::{get_process_pid, run_cmd},
};

const NODE_START_TIMEOUT: Duration = Duration::from_secs(60);
const NODE_RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const NODE_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const NODE_STOPPED_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct Node {
    pub data: NodeData,
    machine: Machine<'static>,
    node_conn: NodeConnection,
}

pub const FC_BIN_NAME: &str = "firecracker";
const FC_BIN_PATH: &str = "/usr/bin/firecracker";
const FC_SOCKET_PATH: &str = "/firecracker.socket";
pub const ROOT_FS_FILE: &str = "os.img";
pub const KERNEL_FILE: &str = "kernel";
const DATA_FILE: &str = "data.img";
pub const VSOCK_PATH: &str = "/vsock.socket";
const VSOCK_GUEST_CID: u32 = 3;
const MAX_KERNEL_ARGS_LEN: usize = 1024;

impl Node {
    /// Creates a new node according to specs.
    #[instrument]
    pub async fn create(data: NodeData) -> Result<Self> {
        let node_id = data.id;
        let config = Node::create_config(&data).await?;
        Node::create_data_image(&node_id, data.babel_conf.requirements.disk_size_gb).await?;
        let machine = Machine::create(config).await?;

        data.save().await?;

        Ok(Self {
            data,
            machine,
            node_conn: NodeConnection::closed(node_id),
        })
    }

    /// Returns node previously created on this host.
    #[instrument]
    pub async fn connect(data: NodeData) -> Result<Self> {
        let config = Node::create_config(&data).await?;
        let cmd = data.id.to_string();
        let (state, node_conn) = match get_process_pid(FC_BIN_NAME, &cmd) {
            Ok(pid) => {
                // Since this is the startup phase it doesn't make sense to wait a long time
                // for the nodes to come online. For that reason we restrict the allowed delay
                // further down.
                let node_conn = NodeConnection::try_open(data.id, NODE_RECONNECT_TIMEOUT).await?;
                debug!("Established babel connection");
                (firec::MachineState::RUNNING { pid }, node_conn)
            }
            Err(_) => (
                firec::MachineState::SHUTOFF,
                NodeConnection::closed(data.id),
            ),
        };
        let machine = Machine::connect(config, state).await;
        Ok(Self {
            data,
            machine,
            node_conn,
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

        Node::copy_os_image(&self.data.id, image).await?;

        self.data.image = image.clone();
        self.data.save().await
    }

    /// Starts the node.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<()> {
        if self.status() == NodeStatus::Running
            || (self.status() == NodeStatus::Failed
                && self.expected_status() == NodeStatus::Stopped)
        {
            info!("Node is recovering, will not start immediately");
            return Ok(());
        }

        self.machine.start().await?;
        self.node_conn = NodeConnection::try_open(self.id(), NODE_START_TIMEOUT).await?;
        self.node_conn
            .babelsup_client()
            .await?
            .setup_supervisor(self.data.babel_conf.supervisor.clone())
            .await?;
        let resp = self.node_conn.babel_rpc(BabelRequest::Ping).await;
        if !matches!(resp, Ok(BabelResponse::Pong)) {
            bail!("Babel Ping request did not respond with `Pong`, but `{resp:?}`")
        }

        // We save the `running` status only after all of the previous steps have succeeded.
        self.data.expected_status = NodeStatus::Running;
        self.data.save().await
    }

    /// Returns the actual status of the node.
    pub fn status(&self) -> NodeStatus {
        self.data.status()
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
            firec::MachineState::RUNNING { .. } => {
                if let Err(err) = self.machine.shutdown().await {
                    warn!("Graceful shutdown failed: {err}");

                    if let Err(err) = self.machine.force_shutdown().await {
                        bail!("Forced shutdown failed: {err}");
                    }
                }
            }
        }

        let start = std::time::Instant::now();
        let elapsed = || std::time::Instant::now() - start;
        loop {
            match get_process_pid(FC_BIN_NAME, &self.data.id.to_string()) {
                Ok(_) if elapsed() < NODE_STOP_TIMEOUT => {
                    debug!("Firecracker process not shutdown yet, will retry");
                    sleep(NODE_STOPPED_CHECK_INTERVAL).await;
                }
                Ok(_) => {
                    bail!("Firecracker shutdown timeout");
                }
                Err(_) => break,
            }
        }

        self.data.expected_status = NodeStatus::Stopped;
        self.data.save().await?;
        self.node_conn = NodeConnection::closed(self.id());

        Ok(())
    }

    /// Deletes the node.
    #[instrument(skip(self))]
    pub async fn delete(self) -> Result<()> {
        self.machine.delete().await?;
        self.data.delete().await
    }

    pub async fn set_self_update(&mut self, self_update: bool) -> Result<()> {
        self.data.self_update = self_update;
        self.data.save().await
    }

    /// Copy OS drive into chroot location.
    async fn copy_os_image(id: &Uuid, image: &NodeImage) -> Result<()> {
        let root_fs_path =
            CookbookService::get_image_download_folder_path(image).join(ROOT_FS_FILE);

        let data_dir = CHROOT_PATH
            .join(FC_BIN_NAME)
            .join(id.to_string())
            .join("root");
        DirBuilder::new().recursive(true).create(&data_dir).await?;

        run_cmd("cp", [root_fs_path.as_os_str(), data_dir.as_os_str()]).await?;

        Ok(())
    }

    /// Create new data drive in chroot location.
    async fn create_data_image(id: &Uuid, disk_size_gb: usize) -> Result<()> {
        let data_dir = CHROOT_PATH
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

    async fn create_config(data: &NodeData) -> Result<firec::config::Config<'static>> {
        let kernel_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on \
            ip={}::{}:255.255.255.240::eth0:on",
            data.network_interface.ip, data.network_interface.gateway,
        );
        if kernel_args.len() > MAX_KERNEL_ARGS_LEN {
            bail!("to long kernel_args {kernel_args}")
        }
        let iface =
            firec::config::network::Interface::new(data.network_interface.name.clone(), "eth0");
        let root_fs_path =
            CookbookService::get_image_download_folder_path(&data.image).join(ROOT_FS_FILE);
        let kernel_path =
            CookbookService::get_image_download_folder_path(&data.image).join(KERNEL_FILE);

        let config = firec::config::Config::builder(Some(data.id), kernel_path)
            // Jailer configuration.
            .jailer_cfg()
            .chroot_base_dir(&*CHROOT_PATH)
            .exec_file(Path::new(FC_BIN_PATH))
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
            .add_drive("data", &*DATA_PATH)
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

    /// Returns the height of the blockchain (in blocks).
    pub async fn height(&mut self) -> Result<u64> {
        self.call_method(&babel_api::BabelMethod::Height, HashMap::new())
            .await
    }

    /// Returns the block age of the blockchain (in seconds).
    pub async fn block_age(&mut self) -> Result<u64> {
        self.call_method(&babel_api::BabelMethod::BlockAge, HashMap::new())
            .await
    }

    /// TODO: Wait for Sean to tell us how to do this.
    pub async fn stake_status(&mut self) -> Result<i32> {
        Ok(0)
    }

    /// Returns the name of the node. This is usually some random generated name that you may use
    /// to recognise the node, but the purpose may vary per blockchain.
    /// ### Example
    /// `chilly-peach-kangaroo`
    pub async fn name(&mut self) -> Result<String> {
        self.call_method(&babel_api::BabelMethod::Name, HashMap::new())
            .await
    }

    /// The address of the node. The meaning of this varies from blockchain to blockchain.
    /// ### Example
    /// `/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3`
    pub async fn address(&mut self) -> Result<String> {
        self.call_method(&babel_api::BabelMethod::Address, HashMap::new())
            .await
    }

    /// Returns whether this node is in consensus or not.
    pub async fn consensus(&mut self) -> Result<bool> {
        self.call_method(&babel_api::BabelMethod::Consensus, HashMap::new())
            .await
    }

    pub async fn init(&mut self, params: HashMap<String, Vec<String>>) -> Result<String> {
        self.call_method(&babel_api::BabelMethod::Init, params)
            .await
    }

    /// This function calls babel by sending a blockchain command using the specified method name.
    pub async fn call_method<T>(
        &mut self,
        name: impl Display + Copy,
        params: HashMap<String, Vec<String>>,
    ) -> Result<T>
    where
        T: FromStr,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        debug!("Calling method: {name}");
        let resp: babel_api::BabelResponse = self
            .node_conn
            .babel_rpc(self.render_method(name, params)?)
            .await?;
        let inner = match resp {
            babel_api::BabelResponse::BlockchainResponse(babel_api::BlockchainResponse {
                value,
            }) => value
                .parse()
                .context(format!("Could not parse {name} response: {value}"))?,
            e => bail!("Unexpected BabelResponse for `{name}`: `{e:?}`"),
        };
        Ok(inner)
    }

    /// Returns the methods that are supported by this blockchain. Calling any method on this
    /// blockchain that is not listed here will result in an error being returned.
    pub async fn capabilities(&mut self) -> Result<Vec<String>> {
        Ok(self
            .data
            .babel_conf
            .methods
            .keys()
            .map(|method| method.to_string())
            .collect())
    }

    /// Checks if node has some particular capability
    pub async fn has_capability(&mut self, method: &str) -> Result<bool> {
        let caps = self.capabilities().await?;
        Ok(caps.contains(&method.to_owned()))
    }

    /// Returns the list of logs from blockchain entry_points.
    pub async fn get_logs(&mut self) -> Result<Vec<String>> {
        let client = self.node_conn.babelsup_client().await?;
        let mut resp = client.get_logs(()).await?.into_inner();
        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = resp.next().await {
            logs.push(log);
        }
        Ok(logs)
    }

    /// Returns blockchain node keys.
    pub async fn download_keys(&mut self) -> Result<Vec<babel_api::BlockchainKey>> {
        let request = babel_api::BabelRequest::DownloadKeys(self.get_keys_config()?);
        let resp: babel_api::BabelResponse = self.node_conn.babel_rpc(request).await?;
        let keys = match resp {
            babel_api::BabelResponse::Keys(keys) => keys,
            e => bail!("Unexpected BabelResponse for `download_keys`: `{e:?}`"),
        };
        Ok(keys)
    }

    /// Sets blockchain node keys.
    pub async fn upload_keys(&mut self, keys: Vec<babel_api::BlockchainKey>) -> Result<()> {
        let request = babel_api::BabelRequest::UploadKeys {
            config: self.get_keys_config()?,
            keys,
        };
        let resp: babel_api::BabelResponse = self.node_conn.babel_rpc(request).await?;
        match resp {
            babel_api::BabelResponse::BlockchainResponse(babel_api::BlockchainResponse {
                value,
            }) => debug!("Upload keys: {value}"),
            e => bail!("Unexpected BabelResponse for `upload_keys`: `{e:?}`"),
        };
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
        self.call_method(&babel_api::BabelMethod::GenerateKeys, HashMap::new())
            .await
    }

    fn render_method(
        &self,
        name: impl Display,
        params: HashMap<String, Vec<String>>,
    ) -> Result<BabelRequest> {
        let get_api_host = |method: &str| -> Result<&String> {
            self.data
                .babel_conf
                .config
                .api_host
                .as_ref()
                .ok_or_else(|| anyhow!("o host specified for method `{method}"))
        };
        Ok(
            match self
                .data
                .babel_conf
                .methods
                .get(&name.to_string())
                .ok_or_else(|| anyhow!("method `{name}` not found"))?
            {
                Jrpc {
                    method, response, ..
                } => {
                    let params = params.into_iter().map(|(k, v)| (k, v.join(","))).collect();
                    babel_api::BabelRequest::BlockchainJrpc {
                        host: get_api_host(method)?.clone(),
                        method: utils::render(method, &params),
                        response: response.clone(),
                    }
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

                    let params = params.into_iter().map(|(k, v)| (k, v.join(","))).collect();
                    babel_api::BabelRequest::BlockchainRest {
                        url: utils::render(&url, &params),
                        response: response.clone(),
                    }
                }
                Sh { body, response, .. } => {
                    // For sh we need to sanitize each param, then join them.
                    let params = params
                        .into_iter()
                        .map(|(k, v)| Ok((k, utils::sanitize_param(&v)?)))
                        .collect::<Result<_>>()?;
                    babel_api::BabelRequest::BlockchainSh {
                        body: utils::render(body, &params),
                        response: response.clone(),
                    }
                }
            },
        )
    }
}
