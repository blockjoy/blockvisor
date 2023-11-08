use crate::{
    babel_engine,
    babel_engine::NodeInfo,
    config::SharedConfig,
    node_connection::RPC_REQUEST_TIMEOUT,
    node_context::NodeContext,
    node_data::{NodeData, NodeImage, NodeStatus},
    pal,
    pal::NodeConnection,
    pal::VirtualMachine,
    pal::{NetInterface, Pal},
    services::cookbook::{CookbookService, ROOT_FS_FILE},
    utils::with_timeout,
};
use babel_api::{
    babelsup::SupervisorConfig,
    metadata::{firewall, BlockchainMetadata},
    rhai_plugin,
    rhai_plugin::RhaiPlugin,
};
use bv_utils::{cmd::run_cmd, with_retry};
use chrono::{DateTime, Utc};
use eyre::{bail, Context, Result};
use std::{fmt::Debug, path::Path, sync::Arc, time::Duration};
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
const MAX_START_TRIES: usize = 3;
const MAX_STOP_TRIES: usize = 3;
const MAX_RECONNECT_TRIES: usize = 3;
const SUPERVISOR_CONFIG: SupervisorConfig = SupervisorConfig {
    backoff_timeout_ms: 3000,
    backoff_base_ms: 200,
};

pub type BabelEngine<N> = babel_engine::BabelEngine<N, RhaiPlugin<babel_engine::Engine>>;

#[derive(Debug)]
pub struct Node<P: Pal> {
    pub data: NodeData<P::NetInterface>,
    pub babel_engine: BabelEngine<P::NodeConnection>,
    metadata: BlockchainMetadata,
    machine: P::VirtualMachine,
    context: NodeContext,
    pal: Arc<P>,
    recovery_counters: RecoveryCounters,
}

#[derive(Debug, Default)]
struct RecoveryCounters {
    reconnect: usize,
    stop: usize,
    start: usize,
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
        let context = NodeContext::build(pal.as_ref(), node_id);
        info!("Creating node with ID: {node_id}");

        let (script, metadata) = context.copy_and_check_plugin(&data.image).await?;

        let _ = tokio::fs::remove_dir_all(&context.data_dir).await;
        context.prepare_data_image::<P>(&data).await?;
        let machine = pal.create_vm(&data).await?;

        data.save(&context.registry).await?;

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
            context.plugin_data.clone(),
        )?;
        Ok(Self {
            data,
            babel_engine,
            metadata,
            machine,
            context,
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
        let context = NodeContext::build(pal.as_ref(), node_id);
        info!("Attaching to node with ID: {node_id}");

        let script = fs::read_to_string(&context.plugin_script).await?;
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
                node_conn.close();
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
            context.plugin_data.clone(),
        )?;
        Ok(Self {
            data,
            babel_engine,
            metadata,
            machine,
            context,
            pal,
            recovery_counters: Default::default(),
        })
    }

    /// Returns the node's `id`.
    pub fn id(&self) -> Uuid {
        self.data.id
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

    /// Returns the expected status of the node.
    pub fn expected_status(&self) -> NodeStatus {
        self.data.expected_status
    }

    pub async fn set_expected_status(&mut self, status: NodeStatus) -> Result<()> {
        self.data.expected_status = status;
        self.data.save(&self.context.registry).await
    }

    pub async fn set_started_at(&mut self, started_at: Option<DateTime<Utc>>) -> Result<()> {
        self.data.started_at = started_at;
        self.data.save(&self.context.registry).await
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
        babel_client
            .setup_babel((id.to_string(), self.metadata.babel_config.clone()))
            .await?;

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
            self.babel_engine.init(Default::default()).await?;
            self.data.initialized = true;
            self.data.save(self.pal.bv_root()).await?;
        }
        // We save the `running` status only after all of the previous steps have succeeded.
        self.set_expected_status(NodeStatus::Running).await?;
        self.set_started_at(Some(Utc::now())).await?;
        debug!("Node started");
        Ok(())
    }

    /// Stops the running node.
    #[instrument(skip(self))]
    pub async fn stop(&mut self, force: bool) -> Result<()> {
        if !force {
            let babel_client = self.babel_engine.node_connection.babel_client().await?;
            let timeout = with_retry!(babel_client.get_babel_shutdown_timeout(()))?.into_inner();
            if let Err(err) = with_retry!(
                babel_client.shutdown_babel(with_timeout((), timeout + RPC_REQUEST_TIMEOUT))
            ) {
                bail!("Failed to gracefully shutdown babel and background jobs: {err:#}");
            }
        }
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
        let _ = fs::remove_file(&self.context.plugin_script).await;
        let _ = fs::remove_file(&self.context.plugin_data).await;
        self.data.delete(&self.context.registry).await
    }

    pub async fn update(&mut self, rules: Vec<firewall::Rule>) -> Result<()> {
        let mut firewall = self.metadata.firewall.clone();
        firewall.rules.append(&mut rules.clone());
        let babel_client = self.babel_engine.node_connection.babel_client().await?;
        with_retry!(babel_client
            .setup_firewall(with_timeout(firewall.clone(), fw_setup_timeout(&firewall))))?;
        self.data.firewall_rules = rules;
        self.data.save(&self.context.registry).await
    }

    /// Updates OS image for VM.
    #[instrument(skip(self))]
    pub async fn upgrade(&mut self, image: &NodeImage) -> Result<()> {
        let need_to_restart = self.status() == NodeStatus::Running;
        if need_to_restart {
            self.stop(false).await?;
        }

        self.copy_os_image(image).await?;

        let (script, metadata) = self.context.copy_and_check_plugin(image).await?;
        self.metadata = metadata;
        self.babel_engine
            .update_plugin(|engine| RhaiPlugin::new(&script, engine))?;

        self.data.image = image.clone();
        self.data.requirements = self.metadata.requirements.clone();
        self.data.initialized = false;
        self.data.save(&self.context.registry).await?;
        self.machine = self.pal.attach_vm(&self.data).await?;

        if need_to_restart {
            self.start().await?;
        }

        debug!("Node upgraded");
        Ok(())
    }

    /// Read script content and update plugin with metadata
    pub async fn reload_plugin(&mut self) -> Result<()> {
        let script = fs::read_to_string(&self.context.plugin_script).await?;
        self.metadata = rhai_plugin::read_metadata(&script)?;
        self.babel_engine
            .update_plugin(|engine| RhaiPlugin::new(&script, engine))
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
                if let Err(e) = self.stop(false).await {
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
                if let Err(e) = self.stop(true).await {
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

    /// Copy OS drive into chroot location.
    async fn copy_os_image(&self, image: &NodeImage) -> Result<()> {
        let root_fs_path =
            CookbookService::get_image_download_folder_path(&self.context.bv_root, image)
                .join(ROOT_FS_FILE);

        let data_dir = &self.context.data_dir;
        fs::create_dir_all(data_dir).await?;

        run_cmd("cp", [root_fs_path.as_os_str(), data_dir.as_os_str()]).await?;

        Ok(())
    }
}

fn fw_setup_timeout(config: &firewall::Config) -> Duration {
    Duration::from_secs(
        FW_SETUP_TIMEOUT_SEC + FW_RULE_SETUP_TIMEOUT_SEC * config.rules.len() as u64,
    )
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{
        config::Config,
        node_context::build_registry_dir,
        pal::{
            BabelClient, BabelSupClient, CommandsStream, NodeConnection, ServiceConnector,
            VirtualMachine, VmState,
        },
        services::{self, cookbook::BABEL_PLUGIN_NAME, AuthToken},
        start_test_server, utils,
        utils::tests::test_channel,
    };
    use assert_fs::TempDir;
    use async_trait::async_trait;
    use babel_api::utils::BinaryStatus;
    use babel_api::{
        babel::BlockchainKey,
        engine::{HttpResponse, JobConfig, JobInfo, JrpcRequest, RestRequest, ShResponse},
        metadata::{BabelConfig, Requirements},
    };
    use mockall::*;
    use serde::{Deserialize, Serialize};
    use std::{
        net::IpAddr,
        path::{Path, PathBuf},
        str::FromStr,
        time::Duration,
    };
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::{transport::Channel, Request, Response, Status, Streaming};

    pub const TEST_KERNEL: &str = "5.10.174-build.1+fc.ufw";
    pub fn testing_babel_path_absolute() -> String {
        format!(
            "{}/../babel_api/protocols/testing/babel.rhai",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    #[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
    pub struct DummyNet {
        pub name: String,
        pub ip: IpAddr,
        pub gateway: IpAddr,
        pub remaster_error: Option<String>,
        pub delete_error: Option<String>,
    }

    #[async_trait]
    impl NetInterface for DummyNet {
        fn name(&self) -> &String {
            &self.name
        }
        fn ip(&self) -> &IpAddr {
            &self.ip
        }
        fn gateway(&self) -> &IpAddr {
            &self.gateway
        }
        async fn remaster(&self) -> Result<()> {
            if let Some(err) = &self.remaster_error {
                bail!(err.clone())
            } else {
                Ok(())
            }
        }
        async fn delete(self) -> Result<()> {
            if let Some(err) = self.delete_error {
                bail!(err)
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    pub struct EmptyStreamConnector;
    pub struct EmptyStream;

    #[async_trait]
    impl ServiceConnector<EmptyStream> for EmptyStreamConnector {
        async fn connect(&self) -> Result<EmptyStream> {
            Ok(EmptyStream)
        }
    }

    #[async_trait]
    impl CommandsStream for EmptyStream {
        async fn wait_for_pending_commands(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    #[derive(Clone)]
    pub struct TestConnector {
        pub tmp_root: PathBuf,
    }

    #[async_trait]
    impl services::ApiServiceConnector for TestConnector {
        async fn connect<T, I>(&self, with_interceptor: I) -> Result<T>
        where
            I: Send + Sync + Fn(Channel, AuthToken) -> T,
        {
            Ok(with_interceptor(
                test_channel(&self.tmp_root),
                AuthToken("test_token".to_owned()),
            ))
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestNodeConnection {}

        #[async_trait]
        impl NodeConnection for TestNodeConnection {
            async fn open(&mut self, _max_delay: Duration) -> Result<()>;
            fn close(&mut self);
            fn is_closed(&self) -> bool;
            fn mark_broken(&mut self);
            fn is_broken(&self) -> bool;
            async fn test(&self) -> Result<()>;
            async fn babelsup_client<'a>(&'a mut self) -> Result<&'a mut BabelSupClient>;
            async fn babel_client<'a>(&'a mut self) -> Result<&'a mut BabelClient>;
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestVM {}

        #[async_trait]
        impl VirtualMachine for TestVM {
            fn state(&self) -> VmState;
            async fn delete(self) -> Result<()>;
            async fn shutdown(&mut self) -> Result<()>;
            async fn force_shutdown(&mut self) -> Result<()>;
            async fn start(&mut self) -> Result<()>;
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestPal {}

        #[tonic::async_trait]
        impl Pal for TestPal {
            fn bv_root(&self) -> &Path;
            fn babel_path(&self) -> &Path;
            fn job_runner_path(&self) -> &Path;
            type NetInterface = DummyNet;
            async fn create_net_interface(
                &self,
                index: u32,
                ip: IpAddr,
                gateway: IpAddr,
                config: &SharedConfig,
            ) -> Result<DummyNet>;

            type CommandsStream = EmptyStream;
            type CommandsStreamConnector = EmptyStreamConnector;
            fn create_commands_stream_connector(
                &self,
                config: &SharedConfig,
            ) -> EmptyStreamConnector;

            type ApiServiceConnector = TestConnector;
            fn create_api_service_connector(&self, config: &SharedConfig) -> TestConnector;

            type NodeConnection = MockTestNodeConnection;
            fn create_node_connection(&self, node_id: Uuid) -> MockTestNodeConnection;

            type VirtualMachine = MockTestVM;
            async fn create_vm(
                &self,
                node_data: &NodeData<DummyNet>,
            ) -> Result<MockTestVM>;
            async fn attach_vm(
                &self,
                node_data: &NodeData<DummyNet>,
            ) -> Result<MockTestVM>;
            fn build_vm_data_path(&self, id: Uuid) -> PathBuf;
        }
    }

    mock! {
        pub TestBabelSupService {}

        #[tonic::async_trait]
        impl babel_api::babelsup::babel_sup_server::BabelSup for TestBabelSupService {
            async fn get_version(&self, request: Request<()>) -> Result<Response<String>, Status>;
            async fn check_babel(
                &self,
                request: Request<u32>,
            ) -> Result<Response<babel_api::utils::BinaryStatus>, Status>;
            async fn start_new_babel(
                &self,
                request: Request<Streaming<babel_api::utils::Binary>>,
            ) -> Result<Response<()>, Status>;
            async fn setup_supervisor(
                &self,
                request: Request<SupervisorConfig>,
            ) -> Result<Response<()>, Status>;
        }
    }

    mock! {
        pub TestBabelService {}

        #[allow(clippy::type_complexity)]
        #[tonic::async_trait]
        impl babel_api::babel::babel_server::Babel for TestBabelService {
            async fn setup_babel(
                &self,
                request: Request<(String, BabelConfig)>,
            ) -> Result<Response<()>, Status>;
            async fn get_babel_shutdown_timeout(
                &self,
                request: Request<()>,
            ) -> Result<Response<Duration>, Status>;
            async fn shutdown_babel(
                &self,
                request: Request<()>,
            ) -> Result<Response<()>, Status>;
            async fn setup_firewall(
                &self,
                request: Request<babel_api::metadata::firewall::Config>,
            ) -> Result<Response<()>, Status>;
            async fn download_keys(
                &self,
                request: Request<babel_api::metadata::KeysConfig>,
            ) -> Result<Response<Vec<BlockchainKey>>, Status>;
            async fn upload_keys(
                &self,
                request: Request<(babel_api::metadata::KeysConfig, Vec<BlockchainKey>)>,
            ) -> Result<Response<String>, Status>;
            async fn check_job_runner(
                &self,
                request: Request<u32>,
            ) -> Result<Response<babel_api::utils::BinaryStatus>, Status>;
            async fn upload_job_runner(
                &self,
                request: Request<Streaming<babel_api::utils::Binary>>,
            ) -> Result<Response<()>, Status>;
            async fn create_job(
                &self,
                request: Request<(String, JobConfig)>,
            ) -> Result<Response<()>, Status>;
            async fn start_job(
                &self,
                request: Request<String>,
            ) -> Result<Response<()>, Status>;
            async fn stop_job(&self, request: Request<String>) -> Result<Response<()>, Status>;
            async fn cleanup_job(&self, request: Request<String>) -> Result<Response<()>, Status>;
            async fn job_info(&self, request: Request<String>) -> Result<Response<JobInfo>, Status>;
            async fn get_jobs(&self, request: Request<()>) -> Result<Response<Vec<(String, JobInfo)>>, Status>;
            async fn run_jrpc(
                &self,
                request: Request<JrpcRequest>,
            ) -> Result<Response<HttpResponse>, Status>;
            async fn run_rest(
                &self,
                request: Request<RestRequest>,
            ) -> Result<Response<HttpResponse>, Status>;
            async fn run_sh(
                &self,
                request: Request<String>,
            ) -> Result<Response<ShResponse>, Status>;
            async fn render_template(
                &self,
                request: Request<(PathBuf, PathBuf, String)>,
            ) -> Result<Response<()>, Status>;
            type GetLogsStream = tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>;
            async fn get_logs(
                &self,
                _request: Request<()>,
            ) -> Result<Response<tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>>, Status>;
            type GetBabelLogsStream = tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>;
            async fn get_babel_logs(
                &self,
                _request: Request<u32>,
            ) -> Result<Response<tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>>, Status>;
        }
    }

    pub fn default_config(bv_root: PathBuf) -> SharedConfig {
        SharedConfig::new(
            Config {
                id: "host_id".to_string(),
                token: "token".to_string(),
                refresh_token: "refresh_token".to_string(),
                blockjoy_api_url: "api.url".to_string(),
                blockjoy_mqtt_url: Some("mqtt.url".to_string()),
                update_check_interval_secs: None,
                blockvisor_port: 888,
                iface: "bvbr7".to_string(),
                cluster_id: None,
                cluster_seed_urls: None,
            },
            bv_root,
        )
    }

    struct TestEnv {
        tmp_root: PathBuf,
        _async_panic_checker: utils::tests::AsyncPanicChecker,
    }

    impl TestEnv {
        async fn new() -> Result<Self> {
            let tmp_root = TempDir::new()?.to_path_buf();

            fs::create_dir_all(build_registry_dir(&tmp_root)).await?;

            Ok(Self {
                tmp_root,
                _async_panic_checker: Default::default(),
            })
        }

        fn default_pal(&self) -> MockTestPal {
            let mut pal = MockTestPal::new();
            pal.expect_bv_root()
                .return_const(self.tmp_root.to_path_buf());
            pal.expect_babel_path()
                .return_const(self.tmp_root.join("babel"));
            pal.expect_job_runner_path()
                .return_const(self.tmp_root.join("job_runner"));
            let tmp_root = self.tmp_root.clone();
            pal.expect_build_vm_data_path()
                .returning(move |id| tmp_root.clone().join(format!("vm_data_{id}")));
            pal.expect_create_commands_stream_connector()
                .return_const(EmptyStreamConnector);
            pal.expect_create_api_service_connector()
                .return_const(TestConnector {
                    tmp_root: self.tmp_root.clone(),
                });
            pal
        }

        fn default_node_data(&self) -> NodeData<DummyNet> {
            NodeData {
                id: Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap(),
                name: "node name".to_string(),
                expected_status: NodeStatus::Stopped,
                started_at: None,
                initialized: false,
                image: NodeImage {
                    protocol: "testing".to_string(),
                    node_type: "validator".to_string(),
                    node_version: "1.2.3".to_string(),
                },
                kernel: TEST_KERNEL.to_string(),
                network_interface: DummyNet {
                    name: "bv1".to_string(),
                    ip: IpAddr::from_str("172.16.0.10").unwrap(),
                    gateway: IpAddr::from_str("172.16.0.1").unwrap(),
                    remaster_error: None,
                    delete_error: None,
                },
                requirements: Requirements {
                    vcpu_count: 1,
                    mem_size_mb: 16,
                    disk_size_gb: 1,
                },
                firewall_rules: vec![],
                properties: Default::default(),
                network: "test".to_string(),
                standalone: true,
            }
        }

        async fn start_server(
            &self,
            babel_sup_mock: MockTestBabelSupService,
            babel_mock: MockTestBabelService,
        ) -> utils::tests::TestServer {
            start_test_server!(
                &self.tmp_root,
                babel_api::babelsup::babel_sup_server::BabelSupServer::new(babel_sup_mock),
                babel_api::babel::babel_server::BabelServer::new(babel_mock)
            )
        }
    }

    fn test_babel_sup_client(tmp_root: &Path) -> &'static mut BabelSupClient {
        // need to leak client, to mock method that return clients reference
        // because of mock used which expect `static lifetime
        Box::leak(Box::new(
            babel_api::babelsup::babel_sup_client::BabelSupClient::with_interceptor(
                test_channel(tmp_root),
                pal::DefaultTimeout(Duration::from_secs(1)),
            ),
        ))
    }

    fn test_babel_client(tmp_root: &Path) -> &'static mut BabelClient {
        // need to leak client, to mock method that return clients reference
        // because of mock used which expect `static lifetime
        Box::leak(Box::new(
            babel_api::babel::babel_client::BabelClient::with_interceptor(
                test_channel(tmp_root),
                pal::DefaultTimeout(Duration::from_secs(1)),
            ),
        ))
    }

    #[tokio::test]
    async fn test_create_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let node_data = test_env.default_node_data();

        pal.expect_create_node_connection()
            .with(predicate::eq(node_data.id))
            .return_once(|_| MockTestNodeConnection::new());
        pal.expect_create_vm()
            .with(predicate::eq(node_data.clone()))
            .once()
            .returning(|_| bail!("create VM error"));
        pal.expect_create_vm()
            .with(predicate::eq(node_data.clone()))
            .once()
            .returning(|_| Ok(MockTestVM::new()));
        let pal = Arc::new(pal);

        assert_eq!(
            "Babel plugin not found for testing/validator/1.2.3",
            Node::create(pal.clone(), config.clone(), node_data.clone())
                .await
                .unwrap_err()
                .to_string()
        );

        let images_dir =
            CookbookService::get_image_download_folder_path(&test_env.tmp_root, &node_data.image);
        fs::create_dir_all(&images_dir).await?;

        fs::write(images_dir.join(BABEL_PLUGIN_NAME), "malformed rhai script").await?;
        assert_eq!(
            "Expecting ';' to terminate this statement (line 1, position 11)",
            Node::create(pal.clone(), config.clone(), node_data.clone())
                .await
                .unwrap_err()
                .to_string()
        );

        fs::copy(
            testing_babel_path_absolute(),
            images_dir.join(BABEL_PLUGIN_NAME),
        )
        .await?;
        assert_eq!(
            "create VM error",
            Node::create(pal.clone(), config.clone(), node_data.clone())
                .await
                .unwrap_err()
                .to_string()
        );

        let node = Node::create(pal, config, node_data).await?;
        assert_eq!(NodeStatus::Stopped, node.expected_status());
        Ok(())
    }

    #[tokio::test]
    async fn test_attach_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut node_data = test_env.default_node_data();
        node_data.expected_status = NodeStatus::Running;
        let mut failed_vm_node_data = node_data.clone();
        failed_vm_node_data.id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047531").unwrap();
        let mut missing_babel_node_data = node_data.clone();
        missing_babel_node_data.id =
            Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047532").unwrap();
        let mut missing_job_runner_node_data = node_data.clone();
        missing_job_runner_node_data.id =
            Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047533").unwrap();
        let config = default_config(test_env.tmp_root.clone());
        let mut pal = test_env.default_pal();

        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection()
            .with(predicate::eq(node_data.id))
            .once()
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_open().return_once(|_| Ok(()));
                let tmp_root = test_tmp_root.clone();
                mock.expect_babelsup_client()
                    .once()
                    .returning(move || Ok(test_babel_sup_client(&tmp_root)));
                let tmp_root = test_tmp_root.clone();
                mock.expect_babel_client()
                    .return_once(move || Ok(test_babel_client(&tmp_root)));
                mock
            });
        pal.expect_create_node_connection()
            .with(predicate::eq(failed_vm_node_data.id))
            .once()
            .returning(move |_| MockTestNodeConnection::new());
        pal.expect_create_node_connection()
            .with(predicate::eq(missing_babel_node_data.id))
            .once()
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_open().return_once(|_| Ok(()));
                mock.expect_close().return_once(|| ());
                mock.expect_is_closed().return_once(|| true);
                mock
            });
        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection()
            .with(predicate::eq(missing_job_runner_node_data.id))
            .once()
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_open().return_once(|_| Ok(()));
                let tmp_root = test_tmp_root.clone();
                mock.expect_babelsup_client()
                    .once()
                    .returning(move || Ok(test_babel_sup_client(&tmp_root)));
                mock.expect_close().return_once(|| ());
                mock.expect_is_closed().return_once(|| true);
                mock
            });
        pal.expect_attach_vm()
            .with(predicate::eq(failed_vm_node_data.clone()))
            .once()
            .returning(|_| bail!("attach VM failed"));
        let missing_job_runner_node_data_id = missing_job_runner_node_data.id;
        let missing_babel_node_data_id = missing_babel_node_data.id;
        let node_data_id = node_data.id;
        pal.expect_attach_vm()
            .withf(move |data| {
                data.id == node_data_id
                    || data.id == missing_babel_node_data_id
                    || data.id == missing_job_runner_node_data_id
            })
            .times(3)
            .returning(|_| {
                let mut mock = MockTestVM::new();
                mock.expect_state().return_const(VmState::RUNNING);
                Ok(mock)
            });
        let mut babel_sup_mock = MockTestBabelSupService::new();
        babel_sup_mock
            .expect_check_babel()
            .returning(|_| Ok(Response::new(BinaryStatus::ChecksumMismatch)));
        babel_sup_mock
            .expect_start_new_babel()
            .returning(|_| Ok(Response::new(())));
        let mut babel_mock = MockTestBabelService::new();
        babel_mock
            .expect_check_job_runner()
            .returning(|_| Ok(Response::new(BinaryStatus::ChecksumMismatch)));
        babel_mock
            .expect_upload_job_runner()
            .returning(|_| Ok(Response::new(())));
        let pal = Arc::new(pal);

        assert_eq!(
            "No such file or directory (os error 2)",
            Node::attach(pal.clone(), config.clone(), node_data.clone())
                .await
                .unwrap_err()
                .to_string()
        );

        let registry_dir = build_registry_dir(&test_env.tmp_root);
        fs::create_dir_all(&registry_dir).await?;
        fs::write(
            registry_dir.join(format!("{}.rhai", node_data.id)),
            "invalid rhai script",
        )
        .await?;
        assert_eq!(
            "Expecting ';' to terminate this statement (line 1, position 9)",
            Node::attach(pal.clone(), config.clone(), node_data.clone())
                .await
                .unwrap_err()
                .to_string()
        );

        fs::copy(
            testing_babel_path_absolute(),
            registry_dir.join(format!("{}.rhai", node_data.id)),
        )
        .await?;
        fs::copy(
            testing_babel_path_absolute(),
            registry_dir.join(format!("{}.rhai", failed_vm_node_data.id)),
        )
        .await?;
        fs::copy(
            testing_babel_path_absolute(),
            registry_dir.join(format!("{}.rhai", missing_babel_node_data.id)),
        )
        .await?;
        fs::copy(
            testing_babel_path_absolute(),
            registry_dir.join(format!("{}.rhai", missing_job_runner_node_data.id)),
        )
        .await?;
        assert_eq!(
            "attach VM failed",
            Node::attach(pal.clone(), config.clone(), failed_vm_node_data)
                .await
                .unwrap_err()
                .to_string()
        );

        let node = Node::attach(pal.clone(), config.clone(), missing_babel_node_data).await?;
        assert_eq!(NodeStatus::Failed, node.status());

        fs::write(&test_env.tmp_root.join("babel"), "dummy babel")
            .await
            .unwrap();
        let node = Node::attach(
            pal.clone(),
            config.clone(),
            missing_job_runner_node_data.clone(),
        )
        .await?;
        assert_eq!(NodeStatus::Failed, node.status());

        fs::write(&test_env.tmp_root.join("job_runner"), "dummy job_runner")
            .await
            .unwrap();
        let server = test_env.start_server(babel_sup_mock, babel_mock).await;
        let node = Node::attach(pal, config, node_data).await?;
        assert_eq!(NodeStatus::Running, node.expected_status());
        server.assert().await;
        Ok(())
    }
}
