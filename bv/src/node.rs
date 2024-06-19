use crate::{
    babel_engine,
    babel_engine::NodeInfo,
    bv_context::BvContext,
    command_failed, commands,
    commands::into_internal,
    config::SharedConfig,
    node_context::NodeContext,
    node_state::{NodeImage, NodeState, NodeStatus},
    pal::{self, NodeConnection, NodeFirewallConfig, Pal, RecoverBackoff, VirtualMachine},
    scheduler,
    services::blockchain::{self, ROOTFS_FILE},
    utils,
};
use babel_api::{
    engine::JobStatus,
    metadata::{firewall, BlockchainMetadata},
    rhai_plugin,
    rhai_plugin::RhaiPlugin,
};
use bv_utils::{cmd::run_cmd, rpc::with_timeout, with_retry};
use chrono::Utc;
use eyre::{anyhow, bail, Context, Report, Result};
use std::{fmt::Debug, path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio::{fs, time::Instant};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

pub const NODE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_UPGRADE_RETRY_HINT: Duration = Duration::from_secs(3600);
const NODE_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const NODE_STOPPED_CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub type BabelEngine<N> = babel_engine::BabelEngine<N, RhaiPlugin<babel_engine::Engine>>;

#[derive(Debug)]
pub struct Node<P: Pal> {
    pub state: NodeState,
    pub babel_engine: BabelEngine<P::NodeConnection>,
    metadata: BlockchainMetadata,
    machine: P::VirtualMachine,
    context: NodeContext,
    pal: Arc<P>,
    recovery_backoff: P::RecoveryBackoff,
    bv_context: BvContext,
}

struct MaybeNode<P: Pal> {
    context: NodeContext,
    state: NodeState,
    machine: Option<P::VirtualMachine>,
    scheduler_tx: mpsc::Sender<scheduler::Action>,
}

macro_rules! check {
    ($res:expr, $self:ident) => {{
        match $res {
            Ok(res) => res,
            Err(err) => return Err((err, $self)),
        }
    }};
}

impl<P: Pal> MaybeNode<P> {
    async fn try_create(
        mut self,
        pal: Arc<P>,
        api_config: SharedConfig,
    ) -> Result<Node<P>, (eyre::Report, Self)> {
        let node_id = self.state.id;
        info!("Creating node with ID: {node_id}");

        let _ = fs::remove_dir_all(&self.context.node_dir).await;
        check!(
            fs::create_dir_all(&self.context.node_dir)
                .await
                .map_err(Report::new),
            self
        );
        let (script, metadata) = check!(
            self.context.copy_and_check_plugin(&self.state.image).await,
            self
        );
        check!(
            pal.apply_firewall_config(NodeFirewallConfig {
                id: self.state.id,
                ip: self.state.network_interface.ip,
                config: build_firewall_rules(&self.state.firewall_rules, &metadata.firewall),
            })
            .await,
            self
        );
        let bv_context = BvContext::from_config(api_config.config.read().await.clone());
        self.machine = Some(check!(pal.create_vm(&bv_context, &self.state).await, self));
        check!(self.state.save(&self.context.nodes_dir).await, self);

        let babel_engine = check!(
            BabelEngine::new(
                NodeInfo {
                    node_id,
                    image: self.state.image.clone(),
                    properties: self.state.properties.clone(),
                    network: self.state.network.clone(),
                },
                pal.create_node_connection(node_id),
                api_config,
                |engine| RhaiPlugin::new(&script, engine),
                self.context.plugin_data.clone(),
                self.scheduler_tx.clone()
            )
            .await,
            self
        );
        let recovery_backoff = pal.create_recovery_backoff();
        Ok(Node {
            state: self.state,
            babel_engine,
            metadata,
            // if we got into that place, then it is safe to unwrap
            machine: self.machine.unwrap(),
            context: self.context,
            pal,
            recovery_backoff,
            bv_context,
        })
    }

    async fn cleanup(mut self) -> Result<()> {
        if let Some(mut machine) = self.machine.take() {
            machine.delete().await?;
        }
        Ok(())
    }
}

impl<P: Pal + Debug> Node<P> {
    /// Creates a new node according to specs.
    #[instrument(skip(pal, api_config))]
    pub async fn create(
        pal: Arc<P>,
        api_config: SharedConfig,
        state: NodeState,
        scheduler_tx: mpsc::Sender<scheduler::Action>,
    ) -> Result<Self> {
        let maybe_node = MaybeNode {
            context: NodeContext::build(pal.as_ref(), state.id),
            state,
            machine: None,
            scheduler_tx,
        };
        match maybe_node.try_create(pal, api_config).await {
            Ok(node) => Ok(node),
            Err((err, maybe_node)) => {
                if let Err(err) = maybe_node.cleanup().await {
                    error!("Cleanup failed after unsuccessful node create: {err:#}");
                }
                Err(err)
            }
        }
    }

    /// Returns node previously created on this host.
    #[instrument(skip(pal, api_config))]
    pub async fn attach(
        pal: Arc<P>,
        api_config: SharedConfig,
        state: NodeState,
        scheduler_tx: mpsc::Sender<scheduler::Action>,
    ) -> Result<Self> {
        let node_id = state.id;
        let context = NodeContext::build(pal.as_ref(), node_id);
        info!("Attaching to node with ID: {node_id}");

        let script = fs::read_to_string(&context.plugin_script).await?;
        let metadata = rhai_plugin::read_metadata(&script)?;

        let mut node_conn = pal.create_node_connection(node_id);
        let bv_context = BvContext::from_config(api_config.config.read().await.clone());
        let machine = pal.attach_vm(&bv_context, &state).await?;
        if machine.state().await == pal::VmState::RUNNING {
            debug!("connecting to babel ...");
            // Since this is the startup phase it doesn't make sense to wait a long time
            // for the nodes to come online. For that reason we restrict the allowed delay
            // further down.
            if let Err(err) = node_conn.attach().await {
                warn!("failed to reestablish babel connection to running node {node_id}: {err:#}");
                node_conn.close();
            } else if let Err(err) = check_job_runner(&mut node_conn, pal.job_runner_path()).await {
                warn!("failed to check/update job runner on running node {node_id}: {err:#}");
                node_conn.close();
            }
        }
        let mut babel_engine = BabelEngine::new(
            NodeInfo {
                node_id,
                image: state.image.clone(),
                properties: state.properties.clone(),
                network: state.network.clone(),
            },
            node_conn,
            api_config,
            |engine| RhaiPlugin::new(&script, engine),
            context.plugin_data.clone(),
            scheduler_tx,
        )
        .await?;
        if state.expected_status == NodeStatus::Running {
            if let Err(err) = babel_engine.start().await {
                error!("failed to start babel engine for node {node_id}: {err:#}");
            }
        }
        let recovery_backoff = pal.create_recovery_backoff();
        Ok(Self {
            state,
            babel_engine,
            metadata,
            machine,
            context,
            pal,
            recovery_backoff,
            bv_context,
        })
    }

    pub async fn detach(&mut self) -> Result<()> {
        self.babel_engine.stop().await
    }

    /// Returns the node's `id`.
    pub fn id(&self) -> Uuid {
        self.state.id
    }

    /// Returns the actual status of the node.
    pub async fn status(&self) -> NodeStatus {
        let machine_status = match self.machine.state().await {
            pal::VmState::RUNNING => NodeStatus::Running,
            pal::VmState::SHUTOFF => NodeStatus::Stopped,
            pal::VmState::INVALID => NodeStatus::Failed,
        };
        if machine_status == self.state.expected_status {
            if machine_status == NodeStatus::Running // node is running, but 
                && (self.babel_engine.node_connection.is_closed() // there is no babel connection
                    || self.babel_engine.node_connection.is_broken() // or is broken for some reason
                    || !self.state.initialized // or it failed to initialize
            ) {
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
        self.state.expected_status
    }

    /// Starts the node.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<()> {
        let status = self.status().await;
        if status == NodeStatus::Failed && self.expected_status() == NodeStatus::Stopped {
            bail!("can't start node which is not stopped properly");
        }
        self.save_expected_status(NodeStatus::Running).await?;
        if status == NodeStatus::Running {
            return Ok(());
        }

        self.babel_engine.start().await?;
        if self.machine.state().await == pal::VmState::SHUTOFF {
            self.machine.start().await?;
        }
        self.babel_engine.node_connection.setup().await?;
        check_job_runner(
            &mut self.babel_engine.node_connection,
            self.pal.job_runner_path(),
        )
        .await?;

        // setup babel
        let babel_client = self.babel_engine.node_connection.babel_client().await?;
        with_retry!(babel_client.setup_babel(self.metadata.babel_config.clone()))?;

        if !self.state.initialized {
            if let Err(err) = self.babel_engine.init().await {
                // mark as permanently failed - non-recoverable
                self.save_expected_status(NodeStatus::Failed).await?;
                return Err(err);
            }
            self.state.initialized = true;
        }
        self.state.started_at = Some(Utc::now());
        self.state.restarting = false;
        self.state.save(&self.context.nodes_dir).await?;
        debug!("Node started");
        Ok(())
    }

    /// Stops the running node.
    #[instrument(skip(self))]
    pub async fn stop(&mut self, force: bool) -> Result<()> {
        self.save_expected_status(NodeStatus::Stopped).await?;
        if self.status().await == NodeStatus::Stopped {
            return Ok(());
        }
        if let Err(err) = self.shutdown_babel(force).await {
            if force {
                warn!("force babel shutdown failed: {err:#}");
            } else {
                bail!("{err:#}")
            }
        }
        self.babel_engine.node_connection.close();
        self.state.started_at = None;
        self.state.save(&self.context.nodes_dir).await?;
        match self.machine.state().await {
            pal::VmState::SHUTOFF => {}
            pal::VmState::RUNNING | pal::VmState::INVALID => {
                if let Err(err) = self.machine.shutdown().await {
                    warn!("Graceful shutdown failed: {err:#}");
                    self.machine
                        .force_shutdown()
                        .await
                        .with_context(|| "Forced shutdown failed")?;
                }
                let start = Instant::now();
                loop {
                    if pal::VmState::SHUTOFF == self.machine.state().await {
                        break;
                    } else if start.elapsed() < NODE_STOP_TIMEOUT {
                        debug!("VM not shutdown yet, will retry");
                        tokio::time::sleep(NODE_STOPPED_CHECK_INTERVAL).await;
                    } else {
                        bail!("VM shutdown timeout");
                    }
                }
            }
        }
        self.babel_engine.stop().await?;
        debug!("Node stopped");
        Ok(())
    }

    pub async fn restart(&mut self, force: bool) -> Result<()> {
        self.state.restarting = true;
        self.stop(force).await?;
        self.start().await?;
        Ok(())
    }

    /// Deletes the node.
    #[instrument(skip(self))]
    pub async fn delete(&mut self) -> Result<()> {
        // set expected to `Stopped` just in case of delete errors
        self.save_expected_status(NodeStatus::Stopped).await?;
        self.babel_engine.stop().await?;
        self.machine.delete().await?;
        self.pal.cleanup_firewall_config(self.state.id).await?;
        self.context.delete().await
    }

    pub async fn update(
        &mut self,
        rules: Vec<firewall::Rule>,
        org_id: String,
    ) -> commands::Result<()> {
        self.state.firewall_rules = rules;
        self.state.org_id = org_id;
        let res = self
            .pal
            .apply_firewall_config(NodeFirewallConfig {
                id: self.state.id,
                ip: self.state.network_interface.ip,
                config: build_firewall_rules(&self.state.firewall_rules, &self.metadata.firewall),
            })
            .await
            .map_err(into_internal);
        if res.is_err() {
            self.state.expected_status = NodeStatus::Failed;
        }
        self.state.save(&self.context.nodes_dir).await?;
        res
    }

    /// Updates OS image for VM.
    #[instrument(skip(self))]
    pub async fn upgrade(&mut self, image: &NodeImage) -> commands::Result<()> {
        let status = self.status().await;
        if status == NodeStatus::Failed {
            return Err(commands::Error::Internal(anyhow!(
                "can't upgrade node in Failed state"
            )));
        }
        let need_to_restart = status == NodeStatus::Running;
        if need_to_restart {
            if self
                .babel_engine
                .get_jobs()
                .await?
                .iter()
                .any(|(_, job)| job.status == JobStatus::Running && job.upgrade_blocking)
            {
                command_failed!(commands::Error::BlockingJobRunning {
                    retry_hint: DEFAULT_UPGRADE_RETRY_HINT,
                });
            }
            self.stop(false).await?;
        }
        self.machine.release().await?;
        self.copy_os_image(image).await?;

        let (script, metadata) = self.context.copy_and_check_plugin(image).await?;
        self.metadata = metadata;
        self.babel_engine
            .update_plugin(|engine| RhaiPlugin::new(&script, engine))
            .await?;
        self.babel_engine.update_node_image(image.clone());
        self.state.image = image.clone();
        self.state.requirements = self.metadata.requirements.clone();
        self.state.initialized = false;
        self.state.save(&self.context.nodes_dir).await?;
        self.machine = self.pal.attach_vm(&self.bv_context, &self.state).await?;

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
            .await
    }

    pub async fn recover(&mut self) -> Result<()> {
        if self.recovery_backoff.backoff() {
            return Ok(());
        }
        let id = self.id();
        match self.state.expected_status {
            NodeStatus::Running => {
                let vm_state = self.machine.state().await;
                if vm_state == pal::VmState::SHUTOFF || !self.state.initialized {
                    self.started_node_recovery().await?;
                } else if vm_state == pal::VmState::INVALID {
                    self.vm_recovery().await?;
                } else {
                    self.node_connection_recovery().await?;
                }
            }
            NodeStatus::Stopped => {
                info!("Recovery: stopping node with ID `{id}`");
                if let Err(e) = self
                    .stop(
                        // it doesn't make sense to try gracefully shutdown node that we can't communicate with,
                        // so force shutdown if node_connection is already closed or broken
                        self.babel_engine.node_connection.is_closed()
                            || self.babel_engine.node_connection.is_broken(),
                    )
                    .await
                {
                    warn!("Recovery: stopping node with ID `{id}` failed: {e:#}");
                    if self.recovery_backoff.stop_failed() {
                        error!("Recovery: retries count exceeded, mark as failed: {e:#}");
                        self.save_expected_status(NodeStatus::Failed).await?;
                    }
                } else {
                    self.post_recovery();
                    if self.state.restarting {
                        self.start().await?;
                    }
                }
            }
            NodeStatus::Failed => {
                warn!("Recovery: node with ID `{id}` cannot be recovered");
            }
            NodeStatus::Busy => unreachable!(),
        }
        Ok(())
    }

    async fn shutdown_babel(&mut self, force: bool) -> Result<()> {
        let babel_client = self.babel_engine.node_connection.babel_client().await?;
        let timeout = with_retry!(babel_client.get_babel_shutdown_timeout(()))?.into_inner();
        with_retry!(
            babel_client.shutdown_babel(with_timeout(force, timeout + NODE_REQUEST_TIMEOUT))
        )
        .with_context(|| "Failed to gracefully shutdown babel and background jobs")?;
        Ok(())
    }

    async fn save_expected_status(&mut self, status: NodeStatus) -> Result<()> {
        self.state.expected_status = status;
        self.state.save(&self.context.nodes_dir).await
    }

    async fn started_node_recovery(&mut self) -> Result<()> {
        let id = self.id();
        info!("Recovery: starting node with ID `{id}`");
        if let Err(e) = self.start().await {
            warn!("Recovery: starting node with ID `{id}` failed: {e:#}");
            if self.recovery_backoff.start_failed() {
                error!("Recovery: retries count exceeded, mark as failed: {e:#}");
                self.save_expected_status(NodeStatus::Failed).await?;
            }
        } else {
            self.post_recovery();
        }
        Ok(())
    }

    async fn node_connection_recovery(&mut self) -> Result<()> {
        let id = self.id();
        info!("Recovery: fix broken connection to node with ID `{id}`");
        if let Err(e) = self.babel_engine.node_connection.test().await {
            warn!("Recovery: reconnect to node with ID `{id}` failed: {e:#}");
            if self.recovery_backoff.reconnect_failed() {
                self.recover_by_restart().await?;
            }
        } else if self.babel_engine.node_connection.is_closed() {
            // node wasn't fully started so proceed with other stuff
            self.started_node_recovery().await?;
        } else {
            self.post_recovery();
        }
        Ok(())
    }

    async fn vm_recovery(&mut self) -> Result<()> {
        let id = self.id();
        info!("Recovery: fix broken node VM with ID `{id}`");
        if let Err(e) = self.machine.recover().await {
            warn!("Recovery: VM with ID `{id}` recovery failed: {e:#}");
            if self.recovery_backoff.vm_recovery_failed() {
                self.recover_by_restart().await?;
            }
        } else {
            self.post_recovery();
        }
        Ok(())
    }

    async fn recover_by_restart(&mut self) -> Result<()> {
        let id = self.id();
        info!("Recovery: restart broken node with ID `{id}`");
        if let Err(e) = self.restart(true).await {
            warn!("Recovery: restart node with ID `{id}` failed: {e:#}");
            if self.recovery_backoff.stop_failed() {
                error!("Recovery: retries count exceeded, mark as failed: {e:#}");
                self.save_expected_status(NodeStatus::Failed).await?;
            }
        } else {
            self.post_recovery();
        }
        Ok(())
    }

    pub fn post_recovery(&mut self) {
        // reset backoff on successful recovery
        self.recovery_backoff.reset();
    }

    /// Copy OS drive into chroot location.
    async fn copy_os_image(&self, image: &NodeImage) -> Result<()> {
        let source_path = blockchain::get_image_download_folder_path(&self.context.bv_root, image)
            .join(ROOTFS_FILE);
        let os_img_path = &self.context.node_dir.join(ROOTFS_FILE);
        if os_img_path.exists() {
            fs::remove_file(os_img_path).await?;
        }
        run_cmd("cp", [source_path.as_os_str(), os_img_path.as_os_str()]).await?;

        Ok(())
    }
}

fn build_firewall_rules(rules: &[firewall::Rule], firewall: &firewall::Config) -> firewall::Config {
    let mut firewall_config = firewall.clone();
    firewall_config.rules.append(&mut rules.to_vec());
    firewall_config
}

async fn check_job_runner(
    connection: &mut impl NodeConnection,
    job_runner_path: &Path,
) -> Result<()> {
    // check and update job_runner
    let (job_runner_bin, checksum) = utils::load_bin(job_runner_path).await?;
    let client = connection.babel_client().await?;
    let job_runner_status = with_retry!(client.check_job_runner(checksum))?.into_inner();
    if job_runner_status != babel_api::utils::BinaryStatus::Ok {
        info!("Invalid or missing JobRunner service on VM, installing new one");
        with_retry!(client.upload_job_runner(tokio_stream::iter(job_runner_bin.clone())))?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::node_state::NetInterface;
    use crate::{
        config::Config,
        node_context,
        node_context::build_nodes_dir,
        nodes_manager,
        pal::{
            BabelClient, CommandsStream, NodeConnection, ServiceConnector, VirtualMachine, VmState,
        },
        scheduler,
        services::{self, blockchain::BABEL_PLUGIN_NAME, ApiInterceptor, AuthToken},
        utils,
    };
    use assert_fs::TempDir;
    use async_trait::async_trait;
    use babel_api::engine::JobsInfo;
    use babel_api::{
        engine::{HttpResponse, JobConfig, JobInfo, JrpcRequest, RestRequest, ShResponse},
        metadata::{BabelConfig, Requirements},
        utils::BinaryStatus,
    };
    use bv_tests_utils::{rpc::test_channel, start_test_server};
    use bv_utils::rpc::DefaultTimeout;
    use chrono::SubsecRound;
    use mockall::*;
    use std::collections::HashMap;
    use std::{
        net::IpAddr,
        path::{Path, PathBuf},
        str::FromStr,
        time::Duration,
    };
    use tonic::{transport::Channel, Request, Response, Status, Streaming};

    pub fn testing_babel_path_absolute() -> String {
        format!("{}/tests/babel.rhai", env!("CARGO_MANIFEST_DIR"))
    }

    #[derive(Debug, Default, Clone)]
    pub struct DummyBackoff {
        reconnect: u32,
        stop: u32,
        start: u32,
        vm: u32,
    }

    impl RecoverBackoff for DummyBackoff {
        fn backoff(&self) -> bool {
            false
        }
        fn reset(&mut self) {}
        fn start_failed(&mut self) -> bool {
            self.start += 1;
            self.start >= 3
        }
        fn stop_failed(&mut self) -> bool {
            self.stop += 1;
            self.stop >= 3
        }
        fn reconnect_failed(&mut self) -> bool {
            self.reconnect += 1;
            self.reconnect >= 3
        }

        fn vm_recovery_failed(&mut self) -> bool {
            self.vm += 1;
            self.vm >= 3
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
        async fn connect<T, I>(&self, with_interceptor: I) -> Result<T, Status>
        where
            I: Send + Sync + Fn(Channel, ApiInterceptor) -> T,
        {
            Ok(with_interceptor(
                test_channel(&self.tmp_root),
                ApiInterceptor(
                    AuthToken("test_token".to_owned()),
                    DefaultTimeout(Duration::from_secs(1)),
                ),
            ))
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestNodeConnection {}

        #[async_trait]
        impl NodeConnection for TestNodeConnection {
            async fn setup(&mut self) -> Result<()>;
            async fn attach(&mut self) -> Result<()>;
            fn close(&mut self);
            fn is_closed(&self) -> bool;
            fn mark_broken(&mut self);
            fn is_broken(&self) -> bool;
            async fn test(&mut self) -> Result<()>;
            async fn babel_client<'a>(&'a mut self) -> Result<&'a mut BabelClient>;
            fn engine_socket_path(&self) -> &Path;
        }
    }

    mock! {
        #[derive(Debug)]
        pub TestVM {}

        #[async_trait]
        impl VirtualMachine for TestVM {
            async fn state(&self) -> VmState;
            async fn delete(&mut self) -> Result<()>;
            async fn shutdown(&mut self) -> Result<()>;
            async fn force_shutdown(&mut self) -> Result<()>;
            async fn start(&mut self) -> Result<()>;
            async fn release(&mut self) -> Result<()>;
            async fn recover(&mut self) -> Result<()>;
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
                bv_context: &BvContext,
                node_state: &NodeState,
            ) -> Result<MockTestVM>;
            async fn attach_vm(
                &self,
                bv_context: &BvContext,
                node_state: &NodeState,
            ) -> Result<MockTestVM>;
            fn available_cpus(&self) -> usize;
            fn available_resources(&self, nodes_data_cache: &nodes_manager::NodesDataCache) -> Result<pal::AvailableResources>;
            fn used_disk_space_correction(&self, nodes_data_cache: &nodes_manager::NodesDataCache) -> Result<u64>;

            type RecoveryBackoff = DummyBackoff;
            fn create_recovery_backoff(&self) -> DummyBackoff;

            async fn apply_firewall_config(
                &self,
                config: NodeFirewallConfig,
            ) -> Result<()>;
            async fn cleanup_firewall_config(&self, id: Uuid) -> Result<()>;
        }
    }

    mock! {
        pub TestBabelService {}

        #[allow(clippy::type_complexity)]
        #[tonic::async_trait]
        impl babel_api::babel::babel_server::Babel for TestBabelService {
            async fn get_version(&self, _request: Request<()>) -> Result<Response<String>, Status>;
            async fn setup_babel(
                &self,
                request: Request<BabelConfig>,
            ) -> Result<Response<()>, Status>;
            async fn get_babel_shutdown_timeout(
                &self,
                request: Request<()>,
            ) -> Result<Response<Duration>, Status>;
            async fn shutdown_babel(
                &self,
                request: Request<bool>,
            ) -> Result<Response<()>, Status>;
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
            async fn get_job_shutdown_timeout(&self, request: Request<String>) -> Result<Response<Duration>, Status>;
            async fn get_jobs(&self, request: Request<()>) -> Result<Response<JobsInfo>, Status>;
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
            async fn is_download_completed(
                &self,
                request: Request<()>,
            ) -> Result<Response<bool>, Status>;
        }
    }

    pub fn dummy_connection_mock(_id: Uuid) -> MockTestNodeConnection {
        let mut mock = MockTestNodeConnection::new();
        mock.expect_engine_socket_path()
            .return_const(Default::default());
        mock
    }

    pub fn default_config(bv_root: PathBuf) -> SharedConfig {
        SharedConfig::new(
            Config {
                id: "host_id".to_string(),
                name: "host_name".to_string(),
                token: "token".to_string(),
                refresh_token: "refresh_token".to_string(),
                blockjoy_api_url: "api.url".to_string(),
                blockjoy_mqtt_url: Some("mqtt.url".to_string()),
                blockvisor_port: 888,
                iface: "bvbr7".to_string(),
                ..Default::default()
            },
            bv_root,
        )
    }

    pub fn default_firewall_config(id: Uuid, ip: IpAddr) -> NodeFirewallConfig {
        NodeFirewallConfig {
            id,
            ip,
            config: firewall::Config {
                default_in: firewall::Action::Deny,
                default_out: firewall::Action::Allow,
                rules: vec![
                    firewall::Rule {
                        name: "Allowed incoming tcp traffic on port".to_string(),
                        action: firewall::Action::Allow,
                        direction: firewall::Direction::In,
                        protocol: Some(firewall::Protocol::Tcp),
                        ips: None,
                        ports: vec![24567],
                    },
                    firewall::Rule {
                        name: "Allowed incoming udp traffic on ip and port".to_string(),
                        action: firewall::Action::Allow,
                        direction: firewall::Direction::In,
                        protocol: Some(firewall::Protocol::Udp),
                        ips: Some("192.168.0.1".to_string()),
                        ports: vec![24567],
                    },
                ],
            },
        }
    }

    pub fn default_bv_context() -> BvContext {
        BvContext {
            id: "host_id".to_string(),
            name: "host_name".to_string(),
            url: "api.url".to_string(),
        }
    }

    pub fn default_pal(tmp_root: PathBuf) -> MockTestPal {
        let mut pal = MockTestPal::new();
        pal.expect_bv_root().return_const(tmp_root.to_path_buf());
        pal.expect_babel_path().return_const(tmp_root.join("babel"));
        pal.expect_job_runner_path()
            .return_const(tmp_root.join("job_runner"));
        pal.expect_create_commands_stream_connector()
            .return_const(EmptyStreamConnector);
        pal.expect_create_api_service_connector()
            .return_const(TestConnector { tmp_root });
        pal.expect_create_recovery_backoff()
            .return_const(DummyBackoff::default());
        pal
    }

    pub async fn make_node_dir(nodes_dir: &Path, id: Uuid) -> PathBuf {
        let node_dir = nodes_dir.join(id.to_string());
        fs::create_dir_all(&node_dir).await.unwrap();
        node_dir
    }

    pub fn add_firewall_expectation(pal: &mut MockTestPal, id: Uuid, ip: IpAddr) {
        pal.expect_apply_firewall_config()
            .with(predicate::eq(default_firewall_config(id, ip)))
            .once()
            .returning(|_| Ok(()));
    }

    struct TestEnv {
        tmp_root: PathBuf,
        nodes_dir: PathBuf,
        tx: mpsc::Sender<scheduler::Action>,
        _async_panic_checker: utils::tests::AsyncPanicChecker,
    }

    impl TestEnv {
        async fn new() -> Result<Self> {
            let (tx, _) = mpsc::channel(16);
            let tmp_root = TempDir::new()?.to_path_buf();
            let nodes_dir = build_nodes_dir(&tmp_root);
            fs::create_dir_all(&nodes_dir).await?;

            Ok(Self {
                tmp_root,
                nodes_dir,
                tx,
                _async_panic_checker: Default::default(),
            })
        }

        fn default_pal(&self) -> MockTestPal {
            default_pal(self.tmp_root.clone())
        }

        fn default_node_state(&self) -> NodeState {
            NodeState {
                id: Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap(),
                name: "node name".to_string(),
                expected_status: NodeStatus::Running,
                started_at: None,
                initialized: true,
                image: NodeImage {
                    protocol: "testing".to_string(),
                    node_type: "validator".to_string(),
                    node_version: "1.2.3".to_string(),
                },
                network_interface: NetInterface {
                    ip: IpAddr::from_str("172.16.0.10").unwrap(),
                    gateway: IpAddr::from_str("172.16.0.1").unwrap(),
                },
                assigned_cpus: vec![0],
                requirements: Requirements {
                    vcpu_count: 1,
                    mem_size_mb: 16,
                    disk_size_gb: 1,
                },
                firewall_rules: vec![],
                properties: Default::default(),
                network: "test".to_string(),
                standalone: true,
                restarting: false,
                org_id: Default::default(),
                apptainer_config: None,
            }
        }

        async fn start_server(
            &self,
            babel_mock: MockTestBabelService,
        ) -> bv_tests_utils::rpc::TestServer {
            start_test_server!(
                &self.tmp_root,
                babel_api::babel::babel_server::BabelServer::new(babel_mock)
            )
        }
        async fn assert_node_state_saved(&self, node_state: &NodeState) {
            let mut node_state = node_state.clone();
            if let Some(time) = &mut node_state.started_at {
                *time = time.trunc_subsecs(0);
            }
            let saved_data =
                NodeState::load(&self.nodes_dir.join(format!("{}/state.json", node_state.id)))
                    .await
                    .unwrap();
            assert_eq!(saved_data, node_state);
        }
    }

    fn test_babel_client(tmp_root: &Path) -> &'static mut BabelClient {
        // need to leak client, to mock method that return clients reference
        // because of mock used which expect `static lifetime
        Box::leak(Box::new(
            babel_api::babel::babel_client::BabelClient::with_interceptor(
                test_channel(tmp_root),
                bv_utils::rpc::DefaultTimeout(Duration::from_secs(1)),
            ),
        ))
    }

    #[tokio::test]
    async fn test_create_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let bv_context = default_bv_context();
        let node_state = test_env.default_node_state();

        pal.expect_apply_firewall_config()
            .with(predicate::eq(default_firewall_config(
                node_state.id,
                node_state.network_interface.ip,
            )))
            .times(2)
            .returning(|_| Ok(()));
        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .return_once(dummy_connection_mock);
        let mut seq = Sequence::new();
        pal.expect_create_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(|_, _| bail!("create VM error"));
        pal.expect_create_vm()
            .with(predicate::eq(bv_context), predicate::eq(node_state.clone()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(MockTestVM::new()));
        let pal = Arc::new(pal);

        assert_eq!(
            "Babel plugin not found for testing/validator/1.2.3",
            Node::create(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone()
            )
            .await
            .unwrap_err()
            .to_string()
        );

        let images_dir =
            blockchain::get_image_download_folder_path(&test_env.tmp_root, &node_state.image);
        fs::create_dir_all(&images_dir).await?;

        fs::write(images_dir.join(BABEL_PLUGIN_NAME), "malformed rhai script").await?;
        assert_eq!(
            "Rhai syntax error",
            Node::create(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone()
            )
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
            Node::create(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone()
            )
            .await
            .unwrap_err()
            .to_string()
        );

        let node = Node::create(pal, config, node_state, test_env.tx.clone()).await?;
        assert_eq!(NodeStatus::Running, node.expected_status());
        test_env.assert_node_state_saved(&node.state).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_attach_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let node_state = test_env.default_node_state();
        let mut failed_vm_node_state = node_state.clone();
        failed_vm_node_state.id = Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047531").unwrap();
        let mut missing_babel_node_state = node_state.clone();
        missing_babel_node_state.id =
            Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047532").unwrap();
        let mut missing_job_runner_node_state = node_state.clone();
        missing_job_runner_node_state.id =
            Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047533").unwrap();
        let config = default_config(test_env.tmp_root.clone());
        let bv_context = default_bv_context();
        let mut pal = test_env.default_pal();

        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .once()
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_attach().return_once(|| Ok(()));
                let tmp_root = test_tmp_root.clone();
                mock.expect_babel_client()
                    .return_once(move || Ok(test_babel_client(&tmp_root)));
                mock.expect_engine_socket_path()
                    .return_const(Default::default());
                mock
            });
        pal.expect_create_node_connection()
            .with(predicate::eq(failed_vm_node_state.id))
            .once()
            .returning(dummy_connection_mock);
        pal.expect_create_node_connection()
            .with(predicate::eq(missing_babel_node_state.id))
            .once()
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_attach().return_once(|| Ok(()));
                mock.expect_close().return_once(|| ());
                mock.expect_is_closed().return_once(|| true);
                mock.expect_engine_socket_path()
                    .return_const(Default::default());
                mock
            });
        pal.expect_create_node_connection()
            .with(predicate::eq(missing_job_runner_node_state.id))
            .once()
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_attach().return_once(|| Ok(()));
                mock.expect_close().return_once(|| ());
                mock.expect_is_closed().return_once(|| true);
                mock.expect_engine_socket_path()
                    .return_const(Default::default());
                mock
            });
        pal.expect_attach_vm()
            .with(
                predicate::eq(bv_context),
                predicate::eq(failed_vm_node_state.clone()),
            )
            .once()
            .returning(|_, _| bail!("attach VM failed"));
        let missing_job_runner_node_state_id = missing_job_runner_node_state.id;
        let missing_babel_node_state_id = missing_babel_node_state.id;
        let node_state_id = node_state.id;
        pal.expect_attach_vm()
            .withf(move |_, data| {
                data.id == node_state_id
                    || data.id == missing_babel_node_state_id
                    || data.id == missing_job_runner_node_state_id
            })
            .times(3)
            .returning(|_, _| {
                let mut mock = MockTestVM::new();
                mock.expect_state().return_const(VmState::RUNNING);
                Ok(mock)
            });
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
            Node::attach(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone(),
            )
            .await
            .unwrap_err()
            .to_string()
        );

        fs::write(
            make_node_dir(&test_env.nodes_dir, node_state.id)
                .await
                .join("babel.rhai"),
            "invalid rhai script",
        )
        .await?;
        assert_eq!(
            "Rhai syntax error",
            Node::attach(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone()
            )
            .await
            .unwrap_err()
            .to_string()
        );

        fs::copy(
            testing_babel_path_absolute(),
            make_node_dir(&test_env.nodes_dir, node_state.id)
                .await
                .join("babel.rhai"),
        )
        .await?;
        fs::copy(
            testing_babel_path_absolute(),
            make_node_dir(&test_env.nodes_dir, failed_vm_node_state.id)
                .await
                .join("babel.rhai"),
        )
        .await?;
        fs::copy(
            testing_babel_path_absolute(),
            make_node_dir(&test_env.nodes_dir, missing_babel_node_state.id)
                .await
                .join("babel.rhai"),
        )
        .await?;
        fs::copy(
            testing_babel_path_absolute(),
            make_node_dir(&test_env.nodes_dir, missing_job_runner_node_state.id)
                .await
                .join("babel.rhai"),
        )
        .await?;
        assert_eq!(
            "attach VM failed",
            Node::attach(
                pal.clone(),
                config.clone(),
                failed_vm_node_state,
                test_env.tx.clone()
            )
            .await
            .unwrap_err()
            .to_string()
        );

        fs::create_dir_all(node_context::build_node_dir(
            pal.bv_root(),
            missing_babel_node_state_id,
        ))
        .await?;
        let node = Node::attach(
            pal.clone(),
            config.clone(),
            missing_babel_node_state,
            test_env.tx.clone(),
        )
        .await?;
        assert_eq!(NodeStatus::Failed, node.status().await);

        fs::create_dir_all(node_context::build_node_dir(
            pal.bv_root(),
            missing_job_runner_node_state_id,
        ))
        .await?;
        fs::write(&test_env.tmp_root.join("babel"), "dummy babel")
            .await
            .unwrap();
        let node = Node::attach(
            pal.clone(),
            config.clone(),
            missing_job_runner_node_state.clone(),
            test_env.tx.clone(),
        )
        .await?;
        assert_eq!(NodeStatus::Failed, node.status().await);

        fs::create_dir_all(node_context::build_node_dir(pal.bv_root(), node_state_id)).await?;
        fs::write(&test_env.tmp_root.join("job_runner"), "dummy job_runner")
            .await
            .unwrap();
        let server = test_env.start_server(babel_mock).await;
        let node = Node::attach(pal, config, node_state, test_env.tx.clone()).await?;
        assert_eq!(NodeStatus::Running, node.expected_status());
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_start_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let mut node_state = test_env.default_node_state();
        node_state
            .properties
            .insert("TESTING_PARAM".to_string(), "any".to_string());

        let images_dir =
            blockchain::get_image_download_folder_path(&test_env.tmp_root, &node_state.image);
        fs::create_dir_all(&images_dir).await?;
        fs::copy(
            testing_babel_path_absolute(),
            images_dir.join(BABEL_PLUGIN_NAME),
        )
        .await?;

        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection().return_once(move |_| {
            let mut mock = MockTestNodeConnection::new();
            mock.expect_is_closed().return_once(|| false);
            mock.expect_is_broken().return_once(|| false);
            mock.expect_setup().times(3).returning(|| Ok(()));
            let tmp_root = test_tmp_root.clone();
            mock.expect_babel_client()
                .returning(move || Ok(test_babel_client(&tmp_root)));
            mock.expect_mark_broken().return_once(|| ());
            mock.expect_engine_socket_path()
                .return_const(Default::default());
            mock
        });
        add_firewall_expectation(&mut pal, node_state.id, node_state.network_interface.ip);
        pal.expect_create_vm().return_once(|_, _| {
            let mut mock = MockTestVM::new();
            let mut seq = Sequence::new();
            // already started
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            // not properly stopped
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            // VM start failed
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            mock.expect_start()
                .once()
                .in_sequence(&mut seq)
                .returning(|| bail!("VM start failed"));
            // init failed
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            // successfully started
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            // successfully started again, but without init
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            Ok(mock)
        });

        let mut node = Node::create(Arc::new(pal), config, node_state, test_env.tx.clone()).await?;
        assert_eq!(NodeStatus::Running, node.expected_status());

        // already started
        node.start().await?;

        node.state.expected_status = NodeStatus::Stopped;
        assert_eq!(
            "can't start node which is not stopped properly",
            node.start().await.unwrap_err().to_string()
        );

        assert_eq!(
            "VM start failed",
            node.start().await.unwrap_err().to_string()
        );

        let mut babel_mock = MockTestBabelService::new();
        babel_mock
            .expect_check_job_runner()
            .times(3)
            .returning(|_| Ok(Response::new(BinaryStatus::Ok)));
        let expected_config = node.metadata.babel_config.clone();
        babel_mock
            .expect_setup_babel()
            .withf(move |req| {
                let config = req.get_ref();
                *config == expected_config
            })
            .times(3)
            .returning(|_| Ok(Response::new(())));
        let mut seq = Sequence::new();
        babel_mock
            .expect_run_sh()
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Err(Status::internal("error on init")));
        babel_mock
            .expect_run_sh()
            .times(2)
            .in_sequence(&mut seq)
            .returning(|_| {
                Ok(Response::new(ShResponse {
                    exit_code: 0,
                    stdout: "".to_string(),
                    stderr: "".to_string(),
                }))
            });
        babel_mock
            .expect_is_download_completed()
            .returning(|_| Ok(Response::new(true)));
        babel_mock
            .expect_start_job()
            .returning(|_| Ok(Response::new(())));
        babel_mock
            .expect_create_job()
            .returning(|_| Ok(Response::new(())));

        fs::write(&test_env.tmp_root.join("babel"), "dummy babel")
            .await
            .unwrap();
        fs::write(&test_env.tmp_root.join("job_runner"), "dummy job_runner")
            .await
            .unwrap();
        let server = test_env.start_server(babel_mock).await;
        node.state.initialized = false;
        let start_err = format!("{:#}", node.start().await.unwrap_err());
        assert!(start_err.starts_with(r#"node_id=4931bafa-92d9-4521-9fc6-a77eee047530: status: Internal, message: "error on init""#));
        assert_eq!(NodeStatus::Failed, node.state.expected_status);
        assert!(!node.state.initialized);
        assert_eq!(None, node.state.started_at);

        // successfully started
        node.start().await?;
        assert_eq!(NodeStatus::Running, node.state.expected_status);
        assert!(node.state.initialized);
        assert!(node.state.started_at.is_some());
        test_env.assert_node_state_saved(&node.state).await;

        // successfully started again, without init, but with pending update
        node.state.expected_status = NodeStatus::Stopped;
        node.start().await?;
        test_env.assert_node_state_saved(&node.state).await;

        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_stop_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let node_state = test_env.default_node_state();

        let images_dir =
            blockchain::get_image_download_folder_path(&test_env.tmp_root, &node_state.image);
        fs::create_dir_all(&images_dir).await?;
        fs::copy(
            testing_babel_path_absolute(),
            images_dir.join(BABEL_PLUGIN_NAME),
        )
        .await?;

        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection().return_once(move |_| {
            let mut mock = MockTestNodeConnection::new();
            mock.expect_is_closed().return_once(|| false);
            mock.expect_is_broken().return_once(|| false);
            let tmp_root = test_tmp_root.clone();
            mock.expect_babel_client()
                .returning(move || Ok(test_babel_client(&tmp_root)));
            mock.expect_mark_broken().return_once(|| ());
            // force stop node in failed state
            mock.expect_close().return_once(|| ());
            mock.expect_engine_socket_path()
                .return_const(Default::default());
            mock
        });
        add_firewall_expectation(&mut pal, node_state.id, node_state.network_interface.ip);
        pal.expect_create_vm().return_once(|_, _| {
            let mut mock = MockTestVM::new();
            let mut seq = Sequence::new();
            // already stopped
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            // failed to gracefully shutdown babel
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            // force stop node in failed state
            mock.expect_state()
                .times(2)
                .in_sequence(&mut seq)
                .return_const(VmState::RUNNING);
            mock.expect_shutdown()
                .once()
                .in_sequence(&mut seq)
                .returning(|| bail!("graceful VM shutdown failed"));
            mock.expect_force_shutdown()
                .once()
                .in_sequence(&mut seq)
                .returning(|| Ok(()));
            mock.expect_state()
                .once()
                .in_sequence(&mut seq)
                .return_const(VmState::SHUTOFF);
            Ok(mock)
        });

        let mut node = Node::create(Arc::new(pal), config, node_state, test_env.tx.clone()).await?;
        assert_eq!(NodeStatus::Running, node.expected_status());

        node.state.expected_status = NodeStatus::Stopped;
        // already stopped
        node.stop(false).await?;

        let now = Utc::now();
        node.state.expected_status = NodeStatus::Running;
        node.state.started_at = Some(now);
        let mut babel_mock = MockTestBabelService::new();
        let mut seq = Sequence::new();
        // failed to gracefully shutdown babel
        babel_mock
            .expect_get_babel_shutdown_timeout()
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(Response::new(Duration::from_secs(1))));
        babel_mock
            .expect_shutdown_babel()
            .withf(|req| !req.get_ref())
            .times(4)
            .in_sequence(&mut seq)
            .returning(|_| Err(Status::internal("can't stop babel")));
        babel_mock
            .expect_get_babel_shutdown_timeout()
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(Response::new(Duration::from_secs(1))));
        babel_mock
            .expect_shutdown_babel()
            .withf(|req| *req.get_ref())
            .times(4)
            .in_sequence(&mut seq)
            .returning(|_| Err(Status::internal("can't stop babel")));

        let server = test_env.start_server(babel_mock).await;
        assert!(node
            .stop(false)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("Failed to gracefully shutdown babel and background jobs"));
        assert_eq!(NodeStatus::Stopped, node.state.expected_status);
        assert_eq!(Some(now), node.state.started_at);

        // force stop node in failed state
        node.stop(true).await?;
        assert_eq!(NodeStatus::Stopped, node.state.expected_status);
        assert_eq!(None, node.state.started_at);
        test_env.assert_node_state_saved(&node.state).await;

        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_update_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let node_state = test_env.default_node_state();

        let images_dir =
            blockchain::get_image_download_folder_path(&test_env.tmp_root, &node_state.image);
        fs::create_dir_all(&images_dir).await?;
        fs::copy(
            testing_babel_path_absolute(),
            images_dir.join(BABEL_PLUGIN_NAME),
        )
        .await?;

        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection().return_once(move |_| {
            let mut mock = MockTestNodeConnection::new();
            mock.expect_is_closed().returning(|| false);
            mock.expect_is_broken().returning(|| false);
            let tmp_root = test_tmp_root.clone();
            mock.expect_babel_client()
                .returning(move || Ok(test_babel_client(&tmp_root)));
            mock.expect_mark_broken().return_once(|| ());
            mock.expect_engine_socket_path()
                .return_const(Default::default());
            mock
        });
        add_firewall_expectation(&mut pal, node_state.id, node_state.network_interface.ip);
        pal.expect_create_vm()
            .return_once(|_, _| Ok(MockTestVM::new()));

        let mut updated_rules =
            default_firewall_config(node_state.id, node_state.network_interface.ip);
        updated_rules.config.rules.push(firewall::Rule {
            name: "new test rule".to_string(),
            action: firewall::Action::Allow,
            direction: firewall::Direction::Out,
            protocol: None,
            ips: None,
            ports: vec![],
        });
        pal.expect_apply_firewall_config()
            .with(predicate::eq(updated_rules))
            .once()
            .returning(|_| Ok(()));
        pal.expect_apply_firewall_config()
            .once()
            .returning(|_| bail!("failed to apply firewall config"));

        let mut node = Node::create(Arc::new(pal), config, node_state, test_env.tx.clone()).await?;
        assert_eq!(NodeStatus::Running, node.expected_status());

        assert!(node.state.firewall_rules.is_empty());
        node.update(
            vec![firewall::Rule {
                name: "new test rule".to_string(),
                action: firewall::Action::Allow,
                direction: firewall::Direction::Out,
                protocol: None,
                ips: None,
                ports: vec![],
            }],
            "org_id".to_string(),
        )
        .await?;
        assert_eq!(1, node.state.firewall_rules.len());
        assert_eq!(
            "new test rule",
            node.state.firewall_rules.first().unwrap().name
        );
        test_env.assert_node_state_saved(&node.state).await;

        assert_eq!(
            "BV internal error: failed to apply firewall config",
            node.update(vec![], "failed_org_id".to_string())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(0, node.state.firewall_rules.len());
        test_env.assert_node_state_saved(&node.state).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_upgrade_node_rejected() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let node_state = test_env.default_node_state();
        let test_image = node_state.image.clone();

        let images_dir =
            blockchain::get_image_download_folder_path(&test_env.tmp_root, &node_state.image);
        fs::create_dir_all(&images_dir).await?;
        fs::copy(
            testing_babel_path_absolute(),
            images_dir.join(BABEL_PLUGIN_NAME),
        )
        .await?;

        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection().return_once(move |_| {
            let mut mock = MockTestNodeConnection::new();
            mock.expect_is_closed().returning(|| false);
            mock.expect_is_broken().returning(|| false);
            let tmp_root = test_tmp_root.clone();
            mock.expect_babel_client()
                .returning(move || Ok(test_babel_client(&tmp_root)));
            mock.expect_mark_broken().return_once(|| ());
            mock.expect_engine_socket_path()
                .return_const(Default::default());
            mock
        });
        add_firewall_expectation(&mut pal, node_state.id, node_state.network_interface.ip);
        pal.expect_create_vm().return_once(|_, _| {
            let mut mock = MockTestVM::new();
            mock.expect_state().once().returning(|| VmState::RUNNING);
            Ok(mock)
        });

        let mut node = Node::create(Arc::new(pal), config, node_state, test_env.tx.clone()).await?;
        assert_eq!(NodeStatus::Running, node.expected_status());

        let mut babel_mock = MockTestBabelService::new();
        // failed to gracefully shutdown babel
        babel_mock.expect_get_jobs().once().returning(|_| {
            Ok(Response::new(HashMap::from_iter([(
                "upgrade_blocking_job_name".to_string(),
                JobInfo {
                    status: JobStatus::Running,
                    progress: None,
                    restart_count: 0,
                    logs: vec![],
                    upgrade_blocking: true,
                },
            )])))
        });

        let server = test_env.start_server(babel_mock).await;

        assert!(node.state.firewall_rules.is_empty());
        assert_eq!(
            "Can't proceed while 'upgrade_blocking' job is running. Try again after 3600 seconds.",
            node.upgrade(&test_image).await.unwrap_err().to_string()
        );

        server.assert().await;
        Ok(())
    }
}
