use anyhow::{bail, Context, Result};
use firec::config::JailerMode;
use firec::Machine;
use futures_util::StreamExt;
use std::{collections::HashMap, path::Path, str::FromStr, time::Duration};
use tokio::{fs::DirBuilder, time::sleep};
use tracing::{debug, instrument, trace};
use uuid::Uuid;

use crate::node_connection::NODE_RECONNECT_TIMEOUT;
use crate::{
    cookbook_service::CookbookService,
    env::*,
    node_connection,
    node_connection::{NodeConnection, NODE_START_TIMEOUT},
    node_data::{NodeData, NodeImage, NodeStatus},
    utils::{get_process_pid, run_cmd},
};

#[derive(Debug)]
pub struct Node {
    pub data: NodeData,
    machine: Machine<'static>,
    node_conn: NodeConnection,
}

// FIXME: Hardcoding everything for now.
pub const FC_BIN_NAME: &str = "firecracker";
const FC_BIN_PATH: &str = "/usr/bin/firecracker";
const FC_SOCKET_PATH: &str = "/firecracker.socket";
pub const ROOT_FS_FILE: &str = "os.img";
pub const KERNEL_FILE: &str = "kernel";
const DATA_FILE: &str = "data.img";
pub const VSOCK_PATH: &str = "/vsock.socket";
const VSOCK_GUEST_CID: u32 = 3;

impl Node {
    /// Creates a new node according to specs.
    #[instrument]
    pub async fn create(data: NodeData) -> Result<Self> {
        let node_id = data.id;
        let config = Node::create_config(&data)?;
        Node::create_data_image(&node_id, data.requirements.disk_size_gb).await?;
        let machine = firec::Machine::create(config).await?;

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
        let config = Node::create_config(&data)?;
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
        let machine = firec::Machine::connect(config, state).await;
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
            return Ok(());
        }

        self.machine.start().await?;
        self.node_conn = NodeConnection::try_open(self.id(), NODE_START_TIMEOUT).await?;

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
                    trace!("Graceful shutdown failed: {err}");

                    if let Err(err) = self.machine.force_shutdown().await {
                        bail!("Forced shutdown failed: {err}");
                    }
                }
                self.node_conn.wait_for_disconnect(&self.id()).await;

                // FIXME: for some reason firecracker socket is not created by
                // consequent start command if we do not wait a bit here
                sleep(Duration::from_secs(10)).await;
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

        run_cmd(
            "cp",
            &[&root_fs_path.to_string_lossy(), &data_dir.to_string_lossy()],
        )
        .await?;

        Ok(())
    }

    /// Create new data drive in chroot location.
    async fn create_data_image(id: &Uuid, disk_size_gb: usize) -> Result<()> {
        let data_dir = CHROOT_PATH
            .join(FC_BIN_NAME)
            .join(id.to_string())
            .join("root");
        DirBuilder::new().recursive(true).create(&data_dir).await?;
        let path = data_dir.join(DATA_FILE).to_string_lossy().into_owned();

        let gb = format!("{disk_size_gb}G");
        run_cmd("fallocate", &["-l", &gb, &path]).await?;
        run_cmd("mkfs.ext4", &[&path]).await?;

        Ok(())
    }

    fn create_config(data: &NodeData) -> Result<firec::config::Config<'static>> {
        let kernel_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on \
            ip={}::{}:255.255.255.240::eth0:on blockvisor.node={}",
            data.network_interface.ip, data.network_interface.gateway, data.id,
        );
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
            .vcpu_count(data.requirements.vcpu_count)
            .mem_size_mib(data.requirements.mem_size_mb as i64)
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
        self.call_method("height", HashMap::new()).await
    }

    /// Returns the block age of the blockchain (in seconds).
    pub async fn block_age(&mut self) -> Result<u64> {
        self.call_method("block_age", HashMap::new()).await
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
        self.call_method("name", HashMap::new()).await
    }

    /// The address of the node. The meaning of this varies from blockchain to blockchain.
    /// ### Example
    /// `/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3`
    pub async fn address(&mut self) -> Result<String> {
        self.call_method("address", HashMap::new()).await
    }

    /// Returns whether this node is in consensus or not.
    pub async fn consensus(&mut self) -> Result<bool> {
        self.call_method("consensus", HashMap::new()).await
    }

    /// This function calls babel by sending a blockchain command using the specified method name.
    pub async fn call_method<T>(
        &mut self,
        method: &str,
        params: HashMap<String, Vec<String>>,
    ) -> Result<T>
    where
        T: FromStr,
        <T as FromStr>::Err: std::error::Error + Send + Sync + 'static,
    {
        let request = babel_api::BabelRequest::BlockchainCommand(babel_api::BlockchainCommand {
            name: method.to_string(),
            params,
        });
        debug!("Calling method: {method}");
        let resp: babel_api::BabelResponse = self.node_conn.babel_rpc(request).await?;
        let inner = match resp {
            babel_api::BabelResponse::BlockchainResponse(babel_api::BlockchainResponse {
                value,
            }) => value
                .parse()
                .context(format!("Could not parse {method}: {value}"))?,
            e => bail!("Unexpected BabelResponse for `{method}`: `{e:?}`"),
        };
        Ok(inner)
    }

    /// Returns the methods that are supported by this blockchain. Calling any method on this
    /// blockchain that is not listed here will result in an error being returned.
    pub async fn capabilities(&mut self) -> Result<Vec<String>> {
        let request = babel_api::BabelRequest::ListCapabilities;
        let resp: babel_api::BabelResponse = self.node_conn.babel_rpc(request).await?;
        let capabilities = match resp {
            babel_api::BabelResponse::ListCapabilities(caps) => caps,
            e => bail!("Unexpected BabelResponse for `capabilities`: `{e:?}`"),
        };
        Ok(capabilities)
    }

    /// Checks if node has some particular capability
    pub async fn has_capability(&mut self, method: &str) -> Result<bool> {
        let caps = self.capabilities().await?;
        Ok(caps.contains(&method.to_owned()))
    }

    /// Returns the list of logs from blockchain entry_points.
    pub async fn get_logs(&mut self) -> Result<Vec<String>> {
        let client = self.node_conn.babelsup_client().await?;
        let mut resp = client
            .get_logs(node_connection::babelsup_pb::GetLogsRequest {})
            .await?
            .into_inner();
        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = resp.next().await {
            logs.push(log.log);
        }
        Ok(logs)
    }

    /// Returns blockchain node keys.
    pub async fn download_keys(&mut self) -> Result<Vec<babel_api::BlockchainKey>> {
        let request = babel_api::BabelRequest::DownloadKeys;
        let resp: babel_api::BabelResponse = self.node_conn.babel_rpc(request).await?;
        let keys = match resp {
            babel_api::BabelResponse::Keys(keys) => keys,
            e => bail!("Unexpected BabelResponse for `download_keys`: `{e:?}`"),
        };
        Ok(keys)
    }

    /// Sets blockchain node keys.
    pub async fn upload_keys(&mut self, keys: Vec<babel_api::BlockchainKey>) -> Result<()> {
        let request = babel_api::BabelRequest::UploadKeys(keys);
        let resp: babel_api::BabelResponse = self.node_conn.babel_rpc(request).await?;
        match resp {
            babel_api::BabelResponse::BlockchainResponse(babel_api::BlockchainResponse {
                value,
            }) => debug!("Upload keys: {value}"),
            e => bail!("Unexpected BabelResponse for `upload_keys`: `{e:?}`"),
        };
        Ok(())
    }

    /// Generates keys on node
    pub async fn generate_keys(&mut self) -> Result<String> {
        self.call_method(
            &babel_api::BabelMethod::GenerateKeys.to_string(),
            HashMap::new(),
        )
        .await
    }
}
