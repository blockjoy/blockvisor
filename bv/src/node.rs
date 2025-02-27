use crate::node_state::{ConfigUpdate, StateBackup};
use crate::{
    babel_engine,
    babel_engine::NodeInfo,
    bv_config::SharedConfig,
    bv_context::BvContext,
    command_failed, commands,
    commands::into_internal,
    cpu_registry::CpuRegistry,
    node_context::NodeContext,
    node_state::{CpuAssignmentUpdate, NodeState, UpgradeState, UpgradeStep, VmStatus},
    pal::{self, NodeConnection, NodeFirewallConfig, Pal, RecoverBackoff, VirtualMachine},
    scheduler,
};
use babel_api::engine::NodeEnv;
use babel_api::{engine::JobStatus, rhai_plugin::RhaiPlugin, utils::BabelConfig};
use bv_utils::{rpc::with_timeout, with_retry};
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
    cpu_registry: CpuRegistry,
    machine: P::VirtualMachine,
    context: NodeContext,
    node_env: NodeEnv,
    bv_context: BvContext,
    pal: Arc<P>,
    recovery_backoff: P::RecoveryBackoff,
}

struct MaybeNode<P: Pal> {
    context: NodeContext,
    state: NodeState,
    machine: Option<P::VirtualMachine>,
    scheduler_tx: mpsc::Sender<scheduler::Action>,
    cpu_registry: CpuRegistry,
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
        let bv_context = BvContext::from_config(
            api_config.config.read().await.clone(),
            self.state.apptainer_config.clone(),
        );
        let vm = check!(pal.create_vm(&bv_context, &self.state).await, self);
        let plugin_path = vm.plugin_path();
        let node_env = vm.node_env();
        self.machine = Some(vm);
        let bridge = bv_context.bridge.clone();
        check!(
            pal.apply_firewall_config(NodeFirewallConfig {
                id: self.state.id,
                ip: self.state.ip,
                bridge: bridge.clone(),
                config: self.state.firewall.clone(),
            })
            .await,
            self
        );
        check!(
            self.state
                .save(&self.context.nodes_dir)
                .await
                .with_context(|| "failed to save node state"),
            self
        );

        let babel_engine = check!(
            BabelEngine::new(
                NodeInfo {
                    node_id,
                    image: self.state.image.clone(),
                    properties: self.state.properties.clone(),
                },
                node_env.clone(),
                pal.create_node_connection(node_id),
                api_config,
                |engine| RhaiPlugin::from_file(plugin_path, engine),
                self.context.clone(),
                self.scheduler_tx.clone()
            )
            .await,
            self
        );
        let recovery_backoff = pal.create_recovery_backoff();
        Ok(Node {
            state: self.state,
            babel_engine,
            cpu_registry: self.cpu_registry,
            // if we got into that place, then it is safe to unwrap
            machine: self.machine.unwrap(),
            context: self.context,
            node_env,
            bv_context,
            pal,
            recovery_backoff,
        })
    }

    async fn cleanup(mut self, pal: Arc<P>) -> Result<()> {
        if let Some(mut machine) = self.machine.take() {
            machine.delete().await?;
        }
        pal.cleanup_firewall_config(self.state.id).await?;
        Ok(())
    }
}

impl<P: Pal + Debug> Node<P> {
    /// Creates a new node according to specs.
    #[instrument(skip(pal, api_config))]
    pub async fn create(
        pal: Arc<P>,
        api_config: SharedConfig,
        mut state: NodeState,
        scheduler_tx: mpsc::Sender<scheduler::Action>,
        cpu_registry: CpuRegistry,
    ) -> Result<Self> {
        state.assigned_cpus = cpu_registry.acquire(state.vm_config.vcpu_count).await?;
        let maybe_node = MaybeNode {
            context: NodeContext::build(pal.bv_root(), state.id),
            state,
            machine: None,
            scheduler_tx,
            cpu_registry,
        };
        match maybe_node.try_create(pal.clone(), api_config).await {
            Ok(node) => Ok(node),
            Err((err, mut maybe_node)) => {
                maybe_node
                    .cpu_registry
                    .release(&mut maybe_node.state.assigned_cpus)
                    .await;
                if let Err(err) = maybe_node.cleanup(pal).await {
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
        cpu_registry: CpuRegistry,
    ) -> Result<Self> {
        let node_id = state.id;
        let context = NodeContext::build(pal.bv_root(), node_id);
        info!("Attaching to node with ID: {node_id}");

        let mut node_conn = pal.create_node_connection(node_id);
        let bv_context = BvContext::from_config(
            api_config.config.read().await.clone(),
            state.apptainer_config.clone(),
        );
        cpu_registry.mark_acquired(&state.assigned_cpus).await;
        if state.upgrade_state.active {
            if let Some(UpgradeStep::CpuAssignment(CpuAssignmentUpdate::ReleasedCpus(cpus))) = state
                .upgrade_state
                .steps
                .iter()
                .find(|item| matches!(item, UpgradeStep::CpuAssignment(_)))
            {
                cpu_registry.mark_acquired(cpus).await;
            }
        }
        let machine = pal
            .attach_vm(&bv_context, &state)
            .await
            .with_context(|| "attach vm failed")?;
        let plugin_path = machine.plugin_path();
        let node_env = machine.node_env();
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
            } else if state.image.uri.starts_with("legacy://") {
                // LEGACY node support - remove once all nodes upgraded
                let babel_client = node_conn.babel_client().await?;
                let babel_config = BabelConfig {
                    node_env: node_env.clone(),
                    ramdisks: state.vm_config.ramdisks.clone(),
                };
                with_retry!(babel_client.setup_babel(babel_config.clone()))?;
            }
        }
        let mut babel_engine = BabelEngine::new(
            NodeInfo {
                node_id,
                image: state.image.clone(),
                properties: state.properties.clone(),
            },
            node_env.clone(),
            node_conn,
            api_config,
            |engine| RhaiPlugin::from_file(plugin_path, engine),
            context.clone(),
            scheduler_tx,
        )
        .await
        .with_context(|| "can't initialize BabelEngine")?;
        if state.expected_status == VmStatus::Running {
            if let Err(err) = babel_engine.start().await {
                error!("failed to start babel engine for node {node_id}: {err:#}");
            }
        }
        let recovery_backoff = pal.create_recovery_backoff();
        Ok(Self {
            state,
            babel_engine,
            cpu_registry,
            machine,
            context,
            node_env,
            bv_context,
            pal,
            recovery_backoff,
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
    pub async fn status(&self) -> VmStatus {
        let machine_status = match self.machine.state().await {
            pal::VmState::RUNNING => VmStatus::Running,
            pal::VmState::SHUTOFF => VmStatus::Stopped,
            pal::VmState::INVALID => VmStatus::Failed,
        };
        if machine_status == self.state.expected_status {
            if machine_status == VmStatus::Running // node is running, but
                && (self.babel_engine.node_connection.is_closed() // there is no babel connection
                    || self.babel_engine.node_connection.is_broken() // or is broken for some reason
                    || !self.state.initialized // or it failed to initialize
            ) {
                VmStatus::Failed
            } else {
                machine_status
            }
        } else {
            VmStatus::Failed
        }
    }

    /// Returns the expected status of the node.
    pub fn expected_status(&self) -> VmStatus {
        self.state.expected_status
    }

    pub async fn save_state(&self) -> Result<()> {
        self.state
            .save(&self.context.nodes_dir)
            .await
            .with_context(|| "failed to save node state")
    }

    /// Starts the node.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<()> {
        let status = self.status().await;
        if status == VmStatus::Failed && self.expected_status() == VmStatus::Stopped {
            bail!("can't start node which is not stopped properly");
        }
        self.save_expected_status(VmStatus::Running).await?;
        if status == VmStatus::Running {
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
        let babel_config = BabelConfig {
            node_env: self.node_env.clone(),
            ramdisks: self.state.vm_config.ramdisks.clone(),
        };
        with_retry!(babel_client.setup_babel(babel_config.clone()))?;

        if !self.state.initialized {
            self.babel_engine.init().await?;
            self.state.initialized = true;
        }
        self.state.started_at = Some(Utc::now());
        self.state.restarting = false;
        self.save_state().await?;
        debug!("Node started");
        Ok(())
    }

    /// Stops the running node.
    #[instrument(skip(self))]
    pub async fn stop(&mut self, force: bool) -> Result<()> {
        self.save_expected_status(VmStatus::Stopped).await?;
        if self.status().await == VmStatus::Stopped {
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
        self.save_state().await?;
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
        self.save_expected_status(VmStatus::Stopped).await?;
        self.babel_engine.stop().await?;
        self.machine.delete().await?;
        self.cpu_registry
            .release(&mut self.state.assigned_cpus)
            .await;
        self.pal.cleanup_firewall_config(self.state.id).await?;
        self.context.delete().await
    }

    pub async fn update(&mut self, config_update: ConfigUpdate) -> commands::Result<()> {
        self.state.image.config_id = config_update.config_id;
        if let Some(display_name) = config_update.new_display_name {
            self.state.display_name = display_name;
        }
        if let Some(org_id) = config_update.new_org_id {
            self.state.org_id = org_id;
        }
        if let Some(org_name) = config_update.new_org_name {
            self.state.org_name = org_name;
        }
        for (k, v) in config_update.new_values {
            self.state.properties.insert(k, v);
        }
        if let Some(config) = config_update.new_firewall {
            self.state.firewall = config;
            let res = self
                .pal
                .apply_firewall_config(NodeFirewallConfig {
                    id: self.state.id,
                    ip: self.state.ip,
                    bridge: self.bv_context.bridge.clone(),
                    config: self.state.firewall.clone(),
                })
                .await
                .map_err(into_internal);
            if res.is_err() {
                self.state.expected_status = VmStatus::Failed;
            }
            self.save_state().await?;
            res?
        }
        self.machine.update_node_env(&self.state);
        self.node_env = self.machine.node_env();
        Ok(())
    }

    /// Updates OS image and related config for VM.
    #[instrument(skip(self))]
    pub async fn upgrade(&mut self, desired_state: NodeState) -> commands::Result<()> {
        let status = self.status().await;
        if status == VmStatus::Failed {
            return Err(commands::Error::Internal(anyhow!(
                "can't upgrade node in Failed state"
            )));
        }

        let data_dir = self.machine.data_dir();
        if !self.state.upgrade_state.active {
            self.state.upgrade_state = UpgradeState::default();
            if status == VmStatus::Running {
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
                self.state.upgrade_state.steps.push(UpgradeStep::Stop);
                self.state.restarting = true;
                self.stop(false).await?;
            }
            self.state.upgrade_state.active = true;
            self.state.upgrade_state.data_stamp = babel_api::utils::protocol_data_stamp(&data_dir)?;
            let mut state_backup: StateBackup = desired_state.into();
            state_backup.swap_state(&mut self.state);
            self.state.upgrade_state.state_backup = Some(state_backup);
            self.save_state().await?;
        }

        if let Some(err) = &self.state.upgrade_state.need_rollback {
            self.rollback(err.clone()).await
        } else if let Err(err) = self.try_upgrade().await {
            let error_str = format!("{err:#}");
            if self.state.upgrade_state.data_stamp
                != babel_api::utils::protocol_data_stamp(&data_dir)?
            {
                command_failed!(commands::Error::NodeUpgradeFailure(error_str, anyhow!("can't rollback node upgrade if 'init' was already started - protocol data could be changed")))
            }
            if let Some(mut state_backup) = self.state.upgrade_state.state_backup.take() {
                state_backup.swap_state(&mut self.state);
                self.state.upgrade_state.state_backup = Some(state_backup);
                self.state.upgrade_state.need_rollback = Some(error_str.clone());
                self.save_state().await?;
                self.rollback(error_str).await
            } else {
                command_failed!(commands::Error::NodeUpgradeFailure(
                    error_str,
                    anyhow!("can't rollback node upgrade - backup not found")
                ))
            }
        } else {
            info!("Node upgraded");
            // some steps may still need cleanup
            if let Some(UpgradeStep::CpuAssignment(CpuAssignmentUpdate::ReleasedCpus(cpus))) = &self
                .state
                .upgrade_state
                .steps
                .iter()
                .find(|item| matches!(item, UpgradeStep::CpuAssignment(_)))
            {
                self.cpu_registry.release(&mut cpus.clone()).await;
            }
            if self.state.upgrade_state.steps.contains(&UpgradeStep::Vm) {
                if let Err(err) = self.machine.drop_backup().await {
                    warn!("failed to cleanup VM backup after upgrade: {err:#}");
                }
            }
            self.state.upgrade_state.active = false;
            self.save_state().await?;
            Ok(())
        }
    }

    #[instrument(skip(self))]
    async fn rollback(&mut self, err: String) -> commands::Result<()> {
        if let Err(rollback_err) = self.try_rollback().await {
            self.state.upgrade_state.active = false;
            self.save_expected_status(VmStatus::Failed).await?;
            command_failed!(commands::Error::NodeUpgradeFailure(err, rollback_err))
        } else {
            self.state.upgrade_state.active = false;
            self.save_state().await?;
            command_failed!(commands::Error::NodeUpgradeRollback(anyhow!(err)))
        }
    }

    #[instrument(skip(self))]
    async fn try_upgrade(&mut self) -> Result<()> {
        if self.state.assigned_cpus.len() != self.state.vm_config.vcpu_count {
            if self.state.assigned_cpus.len() > self.state.vm_config.vcpu_count {
                self.state
                    .upgrade_state
                    .steps
                    .push(UpgradeStep::CpuAssignment(
                        CpuAssignmentUpdate::ReleasedCpus(
                            self.state
                                .assigned_cpus
                                .split_off(self.state.vm_config.vcpu_count),
                        ),
                    ));
            } else {
                self.state
                    .upgrade_state
                    .steps
                    .push(UpgradeStep::CpuAssignment(
                        CpuAssignmentUpdate::AcquiredCpus(self.state.assigned_cpus.len()),
                    ));
                let diff = self.state.vm_config.vcpu_count - self.state.assigned_cpus.len();
                self.state
                    .assigned_cpus
                    .append(&mut self.cpu_registry.acquire(diff).await?);
            }
            self.save_state().await?;
        }
        if self.state.upgrade_state.insert_step(UpgradeStep::Vm) {
            let res = self.machine.upgrade(&self.state).await;
            self.save_state().await?;
            res?;
        }

        if self.state.upgrade_state.insert_step(UpgradeStep::Plugin) {
            let plugin_path = self.machine.plugin_path();
            let node_env = self.machine.node_env();
            self.node_env = node_env.clone();
            self.babel_engine
                .update_node_info(self.state.image.clone(), self.state.properties.clone());
            let res = self
                .babel_engine
                .update_plugin(
                    |engine| RhaiPlugin::from_file(plugin_path, engine),
                    node_env,
                )
                .await;
            self.save_state().await?;
            res?;
        }

        if self.state.upgrade_state.insert_step(UpgradeStep::Firewall) {
            let res = self
                .pal
                .apply_firewall_config(NodeFirewallConfig {
                    id: self.state.id,
                    ip: self.state.ip,
                    bridge: self.bv_context.bridge.clone(),
                    config: self.state.firewall.clone(),
                })
                .await
                .map_err(into_internal);
            self.save_state().await?;
            res?
        }

        if self.state.upgrade_state.steps.contains(&UpgradeStep::Stop) {
            self.start().await?;
        }

        debug!("Node upgraded");
        Ok(())
    }

    #[instrument(skip(self))]
    async fn try_rollback(&mut self) -> Result<()> {
        if self.status().await == VmStatus::Running {
            self.stop(true).await?;
        }
        if let Some(cpus_index) = self
            .state
            .upgrade_state
            .steps
            .iter()
            .position(|item| matches!(item, UpgradeStep::CpuAssignment(_)))
        {
            match self.state.upgrade_state.steps.swap_remove(cpus_index) {
                UpgradeStep::CpuAssignment(CpuAssignmentUpdate::AcquiredCpus(orig_len)) => {
                    self.cpu_registry
                        .release(&mut self.state.assigned_cpus.split_off(orig_len))
                        .await
                }
                UpgradeStep::CpuAssignment(CpuAssignmentUpdate::ReleasedCpus(mut cpus)) => {
                    self.state.assigned_cpus.append(&mut cpus)
                }
                _ => {}
            }
            self.save_state().await?;
        }
        if let Some(vm_index) = self
            .state
            .upgrade_state
            .steps
            .iter()
            .position(|item| matches!(item, UpgradeStep::Vm))
        {
            self.machine.rollback().await?;
            self.state.upgrade_state.steps.swap_remove(vm_index);
            self.save_state().await?;
        }
        if let Some(plugin_index) = self
            .state
            .upgrade_state
            .steps
            .iter()
            .position(|item| matches!(item, UpgradeStep::Plugin))
        {
            let plugin_path = self.machine.plugin_path();
            let node_env = self.machine.node_env();
            self.node_env = node_env.clone();
            self.babel_engine
                .update_node_info(self.state.image.clone(), self.state.properties.clone());
            self.babel_engine
                .update_plugin(
                    |engine| RhaiPlugin::from_file(plugin_path, engine),
                    node_env,
                )
                .await?;
            self.state.upgrade_state.steps.swap_remove(plugin_index);
            self.save_state().await?;
        }
        if let Some(firewall_index) = self
            .state
            .upgrade_state
            .steps
            .iter()
            .position(|item| matches!(item, UpgradeStep::Firewall))
        {
            self.pal
                .apply_firewall_config(NodeFirewallConfig {
                    id: self.state.id,
                    ip: self.state.ip,
                    bridge: self.bv_context.bridge.clone(),
                    config: self.state.firewall.clone(),
                })
                .await
                .map_err(into_internal)?;
            self.state.upgrade_state.steps.swap_remove(firewall_index);
            self.save_state().await?;
        }

        if self.state.upgrade_state.steps.contains(&UpgradeStep::Stop) {
            self.start().await?;
        }
        debug!("Node rolled back");
        Ok(())
    }

    /// Read script content and update plugin with metadata
    pub async fn reload_plugin(&mut self) -> Result<()> {
        let plugin_path = self.machine.plugin_path();
        self.babel_engine
            .update_plugin(
                |engine| RhaiPlugin::from_file(plugin_path, engine),
                self.node_env.clone(),
            )
            .await
    }

    pub async fn recover(&mut self) -> Result<()> {
        if self.recovery_backoff.backoff() {
            return Ok(());
        }
        let id = self.id();
        match self.state.expected_status {
            VmStatus::Running => {
                let vm_state = self.machine.state().await;
                if vm_state == pal::VmState::SHUTOFF || !self.state.initialized {
                    self.started_node_recovery().await?;
                } else if vm_state == pal::VmState::INVALID {
                    self.vm_recovery().await?;
                } else {
                    self.node_connection_recovery().await?;
                }
            }
            VmStatus::Stopped => {
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
                        self.save_expected_status(VmStatus::Failed).await?;
                    }
                } else {
                    self.post_recovery();
                    if self.state.restarting {
                        self.start().await?;
                    }
                }
            }
            VmStatus::Failed => {
                warn!("Recovery: node with ID `{id}` cannot be recovered");
            }
            VmStatus::Busy => unreachable!(),
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

    async fn save_expected_status(&mut self, status: VmStatus) -> Result<()> {
        self.state.expected_status = status;
        self.save_state().await
    }

    async fn started_node_recovery(&mut self) -> Result<()> {
        let id = self.id();
        info!("Recovery: starting node with ID `{id}`");
        if let Err(e) = self.start().await {
            warn!("Recovery: starting node with ID `{id}` failed: {e:#}");
            if self.recovery_backoff.start_failed() {
                error!("Recovery: retries count exceeded, mark as failed: {e:#}");
                self.save_expected_status(VmStatus::Failed).await?;
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
                self.save_expected_status(VmStatus::Failed).await?;
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
}

async fn check_job_runner(
    connection: &mut impl NodeConnection,
    job_runner_path: &Path,
) -> Result<()> {
    // check and update job_runner
    let (job_runner_bin, checksum) = bv_utils::system::load_bin(job_runner_path).await?;
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
    use crate::api_config::ApiConfig;
    use crate::node_state::{NodeImage, ProtocolImageKey, VmConfig};
    use crate::{
        bv_config::Config,
        firewall, node_context,
        node_context::build_nodes_dir,
        nodes_manager,
        pal::{
            BabelClient, CommandsStream, NodeConnection, ServiceConnector, VirtualMachine, VmState,
        },
        scheduler,
        services::{self, ApiInterceptor, AuthToken},
    };
    use assert_fs::TempDir;
    use async_trait::async_trait;
    use babel_api::engine::JobsInfo;
    use babel_api::utils::{BabelConfig, RamdiskConfiguration};
    use babel_api::{
        engine::{HttpResponse, JobConfig, JobInfo, JrpcRequest, NodeEnv, RestRequest, ShResponse},
        utils::BinaryStatus,
    };
    use bv_tests_utils::{rpc::test_channel, start_test_server};
    use bv_utils::rpc::DefaultTimeout;
    use chrono::SubsecRound;
    use core::pin::Pin;
    use mockall::*;
    use std::collections::HashMap;
    use std::time::SystemTime;
    use std::{
        net::IpAddr,
        path::{Path, PathBuf},
        str::FromStr,
        time::Duration,
    };
    use tokio_stream::Stream;
    use tonic::{transport::Channel, Request, Response, Status, Streaming};

    pub fn testing_babel_path_absolute() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/image_v1/main.rhai")
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
            async fn upgrade(&mut self, node_state: &NodeState) -> Result<()>;
            async fn drop_backup(&mut self) -> Result<()>;
            async fn rollback(&mut self) -> Result<()>;
            async fn recover(&mut self) -> Result<()>;
            fn node_env(&self) -> NodeEnv;
            fn update_node_env(&mut self, node_state: &NodeState);
            fn plugin_path(&self) -> PathBuf;
            fn data_dir(&self) -> PathBuf;
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
            async fn available_cpus(&self) -> usize;
            async fn available_resources(&self, nodes_data_cache: nodes_manager::NodesDataCache) -> Result<pal::AvailableResources>;
            async fn used_disk_space_correction(&self, nodes_data_cache: nodes_manager::NodesDataCache) -> Result<u64>;

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
            async fn skip_job(&self, request: Request<String>) -> Result<Response<()>, Status>;
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
            async fn protocol_data_stamp(
                &self,
                request: Request<()>,
            ) -> Result<Response<Option<SystemTime>>, Status>;
            async fn file_write(
                &self,
                request: Request<Streaming<babel_api::utils::Binary>>,
            ) -> Result<Response<()>, Status>;
            type FileReadStream =
                Pin<Box<dyn Stream<Item = Result<babel_api::utils::Binary, Status>> + Send>>;
            async fn file_read(
                &self,
                request: Request<PathBuf>,
            ) -> Result<Response<<Self as babel_api::babel::babel_server::Babel>::FileReadStream>, Status>;
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
                api_config: ApiConfig {
                    token: "token".to_string(),
                    refresh_token: "refresh_token".to_string(),
                    blockjoy_api_url: "api.url".to_string(),
                },
                blockjoy_mqtt_url: Some("mqtt.url".to_string()),
                blockvisor_port: 888,
                iface: "bvbr7".to_string(),
                ..Default::default()
            },
            bv_root,
        )
    }

    pub fn build_firewall_config(state: NodeState) -> NodeFirewallConfig {
        NodeFirewallConfig {
            id: state.id,
            ip: state.ip,
            bridge: Some("bvbr7".to_string()),
            config: state.firewall,
        }
    }

    pub fn default_bv_context() -> BvContext {
        BvContext {
            id: "host_id".to_string(),
            name: "host_name".to_string(),
            url: "api.url".to_string(),
            bridge: Some("bvbr7".to_string()),
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

    pub fn default_node_state() -> NodeState {
        NodeState {
            id: Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap(),
            name: "node name".to_string(),
            protocol_id: "protocol_id".to_string(),
            expected_status: VmStatus::Running,
            started_at: None,
            initialized: true,
            image: NodeImage {
                id: "image_id".to_string(),
                version: "1.2.3".to_string(),
                config_id: "config_id".to_string(),
                archive_id: "archive_id".to_string(),
                store_key: "store_key".to_string(),
                uri: "image.uri".to_string(),
                min_babel_version: "1.0.0".to_string(),
            },
            ip: IpAddr::from_str("172.16.0.10").unwrap(),
            gateway: IpAddr::from_str("172.16.0.1").unwrap(),
            assigned_cpus: vec![3],
            vm_config: VmConfig {
                vcpu_count: 1,
                mem_size_mb: 2048,
                disk_size_gb: 1,
                ramdisks: vec![RamdiskConfiguration {
                    ram_disk_mount_point: "/mnt/ramdisk".to_string(),
                    ram_disk_size_mb: 512,
                }],
            },
            firewall: firewall::Config {
                default_in: firewall::Action::Deny,
                default_out: firewall::Action::Allow,
                rules: vec![
                    firewall::Rule {
                        name: "Allowed incoming tcp traffic on port".to_string(),
                        action: firewall::Action::Allow,
                        direction: firewall::Direction::In,
                        protocol: Some(firewall::Protocol::Tcp),
                        ips: vec!["192.167.0.1/24".to_string()],
                        ports: vec![24567],
                    },
                    firewall::Rule {
                        name: "Allowed incoming udp traffic on ip and port".to_string(),
                        action: firewall::Action::Allow,
                        direction: firewall::Direction::In,
                        protocol: Some(firewall::Protocol::Udp),
                        ips: vec!["192.168.0.1".to_string()],
                        ports: vec![24567],
                    },
                ],
            },
            properties: HashMap::from_iter([(
                "arbitrary-text-property".to_string(),
                "any".to_string(),
            )]),
            dev_mode: false,
            restarting: false,
            org_id: "org_id".to_string(),
            org_name: "org_name".to_string(),
            protocol_name: "testing protocol".to_string(),
            image_key: ProtocolImageKey {
                protocol_key: "testing_protocol".to_string(),
                variant_key: "tst".to_string(),
            },
            dns_name: "dns.name".to_string(),
            apptainer_config: None,
            display_name: "node display name".to_string(),
            upgrade_state: Default::default(),
            tags: vec![],
        }
    }

    pub async fn make_node_dir(nodes_dir: &Path, id: Uuid) -> PathBuf {
        let node_dir = nodes_dir.join(id.to_string());
        fs::create_dir_all(&node_dir).await.unwrap();
        node_dir
    }

    pub fn add_firewall_expectation(pal: &mut MockTestPal, state: NodeState) {
        pal.expect_apply_firewall_config()
            .with(predicate::eq(build_firewall_config(state)))
            .once()
            .returning(|_| Ok(()));
    }

    struct TestEnv {
        tmp_root: PathBuf,
        nodes_dir: PathBuf,
        default_plugin_path: PathBuf,
        node_env: NodeEnv,
        tx: mpsc::Sender<scheduler::Action>,
        _async_panic_checker: bv_tests_utils::AsyncPanicChecker,
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
                default_plugin_path: testing_babel_path_absolute(),
                node_env: NodeEnv {
                    node_version: "test".to_string(),
                    node_protocol: "testing".to_string(),
                    dev_mode: true,
                    ..Default::default()
                },
                tx,
                _async_panic_checker: Default::default(),
            })
        }

        fn default_pal(&self) -> MockTestPal {
            default_pal(self.tmp_root.clone())
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

        fn add_create_node_expectations(
            &self,
            pal: &mut MockTestPal,
            state: NodeState,
            vm_mock: MockTestVM,
        ) {
            let test_tmp_root = self.tmp_root.to_path_buf();
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
            add_firewall_expectation(pal, state.clone());
            pal.expect_create_vm().return_once(move |_, _| Ok(vm_mock));
        }

        fn add_plugin_update_expectations(&self, vm_mock: &mut MockTestVM) {
            let plugin_path = self.default_plugin_path.clone();
            vm_mock
                .expect_plugin_path()
                .once()
                .returning(move || plugin_path.clone());
            vm_mock.expect_node_env().once().returning(Default::default);
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

    fn default_cpu_registry() -> CpuRegistry {
        CpuRegistry::new(4)
    }

    #[tokio::test]
    async fn test_create_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let bv_context = default_bv_context();
        let node_state = default_node_state();

        pal.expect_cleanup_firewall_config().returning(|_| Ok(()));
        let mut seq = Sequence::new();

        pal.expect_create_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(|_, _| bail!("create VM error"));

        let malformed_rhai_path = test_env.tmp_root.join("malformed.rhai");
        fs::write(&malformed_rhai_path, "malformed rhai script")
            .await
            .unwrap();
        pal.expect_create_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(move |_, _| {
                let mut vm = MockTestVM::new();
                let malformed_rhai_path = malformed_rhai_path.clone();
                vm.expect_plugin_path()
                    .returning(move || malformed_rhai_path.clone());
                vm.expect_node_env().returning(Default::default);
                vm.expect_delete().returning(|| Ok(()));
                Ok(vm)
            });
        pal.expect_apply_firewall_config()
            .with(predicate::eq(build_firewall_config(node_state.clone())))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(()));
        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .once()
            .in_sequence(&mut seq)
            .returning(dummy_connection_mock);

        let empty_rhai_path = test_env.tmp_root.join("empty.rhai");
        fs::write(&empty_rhai_path, "").await.unwrap();
        pal.expect_create_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(move |_, _| {
                let mut vm = MockTestVM::new();
                let empty_rhai_path = empty_rhai_path.clone();
                vm.expect_plugin_path()
                    .returning(move || empty_rhai_path.clone());
                vm.expect_node_env().returning(Default::default);
                vm.expect_delete().returning(|| Ok(()));
                Ok(vm)
            });
        pal.expect_apply_firewall_config()
            .with(predicate::eq(build_firewall_config(node_state.clone())))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| bail!("FW apply error"));

        let plugin_path = test_env.default_plugin_path.clone();
        pal.expect_create_vm()
            .with(predicate::eq(bv_context), predicate::eq(node_state.clone()))
            .once()
            .in_sequence(&mut seq)
            .returning(move |_, _| {
                let mut vm = MockTestVM::new();
                let plugin_path = plugin_path.clone();
                vm.expect_plugin_path()
                    .returning(move || plugin_path.clone());
                vm.expect_node_env().returning(Default::default);
                Ok(vm)
            });
        pal.expect_apply_firewall_config()
            .with(predicate::eq(build_firewall_config(node_state.clone())))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(()));
        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .once()
            .in_sequence(&mut seq)
            .returning(dummy_connection_mock);
        let pal = Arc::new(pal);

        assert_eq!(
            "create VM error",
            Node::create(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone(),
                default_cpu_registry(),
            )
            .await
            .unwrap_err()
            .to_string()
        );

        assert_eq!(
            "Rhai syntax error",
            Node::create(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone(),
                default_cpu_registry(),
            )
            .await
            .unwrap_err()
            .to_string()
        );

        assert_eq!(
            "FW apply error",
            Node::create(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone(),
                default_cpu_registry(),
            )
            .await
            .unwrap_err()
            .to_string()
        );

        let node = Node::create(
            pal,
            config,
            node_state,
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Running, node.expected_status());
        test_env.assert_node_state_saved(&node.state).await;
        Ok(())
    }

    #[tokio::test]
    async fn test_attach_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let config = default_config(test_env.tmp_root.clone());
        let bv_context = default_bv_context();
        let mut pal = test_env.default_pal();
        let node_state = default_node_state();

        let mut seq = Sequence::new();

        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .once()
            .in_sequence(&mut seq)
            .returning(dummy_connection_mock);
        pal.expect_attach_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(|_, _| bail!("attach VM failed"));

        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .once()
            .in_sequence(&mut seq)
            .returning(dummy_connection_mock);

        let malformed_rhai_path = test_env.tmp_root.join("malformed.rhai");
        fs::write(&malformed_rhai_path, "malformed rhai script")
            .await
            .unwrap();
        pal.expect_attach_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(move |_, _| {
                let mut vm = MockTestVM::new();
                vm.expect_state().returning(|| pal::VmState::SHUTOFF);
                let malformed_rhai_path = malformed_rhai_path.clone();
                vm.expect_plugin_path()
                    .returning(move || malformed_rhai_path.clone());
                vm.expect_node_env().returning(Default::default);
                Ok(vm)
            });

        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .once()
            .in_sequence(&mut seq)
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_attach().return_once(|| Ok(()));
                mock.expect_close().return_once(|| ());
                mock.expect_is_closed().return_once(|| true);
                mock.expect_engine_socket_path()
                    .return_const(Default::default());
                mock
            });
        let plugin_path = test_env.default_plugin_path.clone();
        pal.expect_attach_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(move |_, _| {
                let mut vm = MockTestVM::new();
                vm.expect_state().returning(|| pal::VmState::RUNNING);
                let plugin_path = plugin_path.clone();
                vm.expect_plugin_path()
                    .returning(move || plugin_path.clone());
                vm.expect_node_env().returning(Default::default);
                Ok(vm)
            });

        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection()
            .with(predicate::eq(node_state.id))
            .once()
            .in_sequence(&mut seq)
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
        let plugin_path = test_env.default_plugin_path.clone();
        pal.expect_attach_vm()
            .with(
                predicate::eq(bv_context.clone()),
                predicate::eq(node_state.clone()),
            )
            .once()
            .in_sequence(&mut seq)
            .returning(move |_, _| {
                let mut vm = MockTestVM::new();
                vm.expect_state().returning(|| pal::VmState::RUNNING);
                let plugin_path = plugin_path.clone();
                vm.expect_plugin_path()
                    .returning(move || plugin_path.clone());
                vm.expect_node_env().returning(Default::default);
                Ok(vm)
            });
        let mut babel_mock = MockTestBabelService::new();
        babel_mock
            .expect_check_job_runner()
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(Response::new(BinaryStatus::ChecksumMismatch)));
        babel_mock
            .expect_upload_job_runner()
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(Response::new(())));

        let pal = Arc::new(pal);

        assert_eq!(
            "attach VM failed",
            Node::attach(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone(),
                default_cpu_registry(),
            )
            .await
            .unwrap_err()
            .root_cause()
            .to_string()
        );

        assert_eq!(
            "Syntax error: Expecting ';' to terminate this statement (line 1, position 11)",
            Node::attach(
                pal.clone(),
                config.clone(),
                node_state.clone(),
                test_env.tx.clone(),
                default_cpu_registry(),
            )
            .await
            .unwrap_err()
            .root_cause()
            .to_string()
        );

        let node = Node::attach(
            pal.clone(),
            config.clone(),
            node_state.clone(),
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Failed, node.status().await);

        fs::create_dir_all(node_context::build_node_dir(pal.bv_root(), node_state.id)).await?;
        fs::write(&test_env.tmp_root.join("job_runner"), "dummy job_runner")
            .await
            .unwrap();
        let server = test_env.start_server(babel_mock).await;
        let node = Node::attach(
            pal,
            config,
            node_state,
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Running, node.expected_status());
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_start_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let node_state = default_node_state();

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
        add_firewall_expectation(&mut pal, node_state.clone());
        let plugin_path = test_env.default_plugin_path.clone();
        let node_env = test_env.node_env.clone();
        pal.expect_create_vm().return_once(move |_, _| {
            let mut mock = MockTestVM::new();
            let plugin_path = plugin_path.clone();
            let mut seq = Sequence::new();
            mock.expect_plugin_path()
                .once()
                .in_sequence(&mut seq)
                .returning(move || plugin_path.clone());
            let node_env = node_env.clone();
            mock.expect_node_env()
                .once()
                .in_sequence(&mut seq)
                .returning(move || node_env.clone());
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

        let mut node = Node::create(
            Arc::new(pal),
            config,
            node_state.clone(),
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Running, node.expected_status());

        // already started
        node.start().await?;

        node.state.expected_status = VmStatus::Stopped;
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
        let expected_config = BabelConfig {
            node_env: test_env.node_env.clone(),
            ramdisks: node_state.vm_config.ramdisks.clone(),
        };
        babel_mock
            .expect_setup_babel()
            .withf(move |req| {
                let config = req.get_ref();
                *config == expected_config
            })
            .times(3)
            .returning(|_| Ok(Response::new(())));
        babel_mock
            .expect_run_sh()
            .withf(|req| req.get_ref() == "echo ok")
            .returning(|_| {
                Ok(Response::new(ShResponse {
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: "".to_string(),
                }))
            });
        let mut seq = Sequence::new();
        babel_mock
            .expect_run_sh()
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Err(Status::internal("error on init")));
        babel_mock
            .expect_run_sh()
            .once()
            .in_sequence(&mut seq)
            .returning(|_| {
                Ok(Response::new(ShResponse {
                    exit_code: 0,
                    stdout: "".to_string(),
                    stderr: "".to_string(),
                }))
            });
        babel_mock
            .expect_protocol_data_stamp()
            .returning(|_| Ok(Response::new(Some(SystemTime::now()))));
        babel_mock
            .expect_start_job()
            .returning(|_| Ok(Response::new(())));
        babel_mock
            .expect_create_job()
            .returning(|_| Ok(Response::new(())));

        fs::write(&test_env.tmp_root.join("job_runner"), "dummy job_runner")
            .await
            .unwrap();
        let server = test_env.start_server(babel_mock).await;
        node.state.initialized = false;
        let start_err = format!("{:#}", node.start().await.unwrap_err());
        assert!(start_err.starts_with(r#"node_id=4931bafa-92d9-4521-9fc6-a77eee047530: status: Internal, message: "error on init""#));
        assert_eq!(VmStatus::Running, node.state.expected_status);
        assert!(!node.state.initialized);
        assert_eq!(None, node.state.started_at);

        // successfully started
        node.start().await?;
        assert_eq!(VmStatus::Running, node.state.expected_status);
        assert!(node.state.initialized);
        assert!(node.state.started_at.is_some());
        test_env.assert_node_state_saved(&node.state).await;

        // successfully started again, without init, but with pending update
        node.state.expected_status = VmStatus::Stopped;
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
        let node_state = default_node_state();

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
        add_firewall_expectation(&mut pal, node_state.clone());
        let plugin_path = test_env.default_plugin_path.clone();
        pal.expect_create_vm().return_once(move |_, _| {
            let mut mock = MockTestVM::new();
            let plugin_path = plugin_path.clone();
            let mut seq = Sequence::new();
            mock.expect_plugin_path()
                .once()
                .in_sequence(&mut seq)
                .returning(move || plugin_path.clone());
            mock.expect_node_env()
                .once()
                .in_sequence(&mut seq)
                .returning(Default::default);
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

        let mut node = Node::create(
            Arc::new(pal),
            config,
            node_state,
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Running, node.expected_status());

        node.state.expected_status = VmStatus::Stopped;
        // already stopped
        node.stop(false).await?;

        let now = Utc::now();
        node.state.expected_status = VmStatus::Running;
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
        assert_eq!(VmStatus::Stopped, node.state.expected_status);
        assert_eq!(Some(now), node.state.started_at);

        // force stop node in failed state
        node.stop(true).await?;
        assert_eq!(VmStatus::Stopped, node.state.expected_status);
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
        let node_state = default_node_state();

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
        add_firewall_expectation(&mut pal, node_state.clone());
        let plugin_path = test_env.default_plugin_path.clone();
        pal.expect_create_vm().return_once(move |_, _| {
            let mut mock = MockTestVM::new();
            let plugin_path = plugin_path.clone();
            mock.expect_plugin_path()
                .once()
                .returning(move || plugin_path.clone());
            mock.expect_node_env().returning(Default::default);
            mock.expect_update_node_env().returning(|_| ());
            Ok(mock)
        });

        let mut updated_config = build_firewall_config(node_state.clone());
        updated_config.config.rules.push(firewall::Rule {
            name: "new test rule".to_string(),
            action: firewall::Action::Allow,
            direction: firewall::Direction::Out,
            protocol: None,
            ips: vec![],
            ports: vec![],
        });
        let updated_firewall = updated_config.config.clone();
        pal.expect_apply_firewall_config()
            .with(predicate::eq(updated_config))
            .once()
            .returning(|_| Ok(()));
        pal.expect_apply_firewall_config()
            .once()
            .returning(|_| bail!("failed to apply firewall config"));

        let mut node = Node::create(
            Arc::new(pal),
            config,
            node_state.clone(),
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Running, node.expected_status());

        assert_eq!(node.state.firewall, node_state.firewall);
        node.update(ConfigUpdate {
            config_id: "new-cfg_id".to_string(),
            new_display_name: Some("new name".to_string()),
            new_firewall: Some(updated_firewall.clone()),
            new_org_id: Some("new org_id".to_string()),
            new_org_name: Some("org name".to_string()),
            new_values: HashMap::from_iter([
                ("new_key1".to_string(), "new value ".to_string()),
                ("new_key2".to_string(), "new value 2".to_string()),
            ]),
        })
        .await?;
        assert_eq!(node.state.firewall, updated_firewall);
        assert_eq!(node.state.display_name, "new name".to_string());
        assert_eq!(node.state.org_id, "new org_id".to_string());
        assert_eq!(node.state.org_name, "org name".to_string());
        assert_eq!(
            node.state.properties.get("new_key1"),
            Some("new value ".to_string()).as_ref()
        );
        test_env.assert_node_state_saved(&node.state).await;

        assert_eq!(
            "BV internal error: 'failed to apply firewall config'",
            node.update(ConfigUpdate {
                config_id: "new-cfg_id".to_string(),
                new_display_name: None,
                new_firewall: Some(firewall::Config::default()),
                new_org_id: Some("failed_org_id".to_string()),
                new_org_name: None,
                new_values: Default::default(),
            },)
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(0, node.state.firewall.rules.len());
        test_env.assert_node_state_saved(&node.state).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_upgrade_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let mut node_state = default_node_state();
        node_state.assigned_cpus = vec![4];
        node_state.expected_status = VmStatus::Stopped;
        let mut new_state = node_state.clone();
        new_state.image.id = "new-image-id".to_string();
        new_state.image.uri = "new.uri".to_string();
        new_state.image.version = "3.2.1".to_string();
        new_state.vm_config.vcpu_count = 5;
        new_state.vm_config.mem_size_mb = 4096;
        new_state.vm_config.disk_size_gb = 3;
        new_state.assigned_cpus = vec![3, 2];
        new_state.firewall.rules.pop();
        new_state
            .properties
            .insert("new-key".to_string(), "new_value".to_string());
        let mut vm_mock = MockTestVM::new();
        let data_dir = test_env.tmp_root.clone();
        vm_mock
            .expect_data_dir()
            .returning(move || data_dir.clone());
        test_env.add_plugin_update_expectations(&mut vm_mock);
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
        add_firewall_expectation(&mut pal, node_state.clone());

        // not enough cpu cores
        vm_mock
            .expect_state()
            .times(2)
            .returning(|| VmState::SHUTOFF);

        // vm failure
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock
            .expect_upgrade()
            .once()
            .returning(|_| bail!("vm upgrade failed"));
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock
            .expect_rollback()
            .once()
            .returning(|| bail!("vm rollback failed"));

        // plugin failure
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_upgrade().once().returning(|_| Ok(()));
        vm_mock
            .expect_plugin_path()
            .once()
            .returning(move || PathBuf::from("new/invalid/path"));
        vm_mock.expect_node_env().once().returning(Default::default);
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_rollback().once().returning(|| Ok(()));
        vm_mock
            .expect_plugin_path()
            .once()
            .returning(move || PathBuf::from("old/invalid/path"));
        vm_mock.expect_node_env().once().returning(Default::default);

        // firewall failure
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_upgrade().once().returning(|_| Ok(()));
        test_env.add_plugin_update_expectations(&mut vm_mock);
        pal.expect_apply_firewall_config()
            .once()
            .returning(|_| bail!("firewall failure"));
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_rollback().once().returning(|| Ok(()));
        test_env.add_plugin_update_expectations(&mut vm_mock);
        pal.expect_apply_firewall_config()
            .once()
            .returning(|_| bail!("firewall rollback failure"));

        // successful upgrade and finalize
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_upgrade().once().returning(|_| Ok(()));
        let plugin_path = test_env.default_plugin_path.clone();
        vm_mock
            .expect_plugin_path()
            .once()
            .returning(move || plugin_path.clone());
        vm_mock.expect_node_env().returning(|| NodeEnv {
            node_name: "some_new_name".to_string(),
            ..Default::default()
        });
        add_firewall_expectation(&mut pal, new_state.clone());
        vm_mock.expect_drop_backup().once().returning(|| Ok(()));

        pal.expect_create_vm().return_once(move |_, _| Ok(vm_mock));
        let mut node = Node::create(
            Arc::new(pal),
            config,
            node_state.clone(),
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Stopped, node.expected_status());

        assert_eq!(
            "node upgrade failed with: 'not enough cpu cores'; but then successfully rolled back",
            node.upgrade(new_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        new_state.vm_config.vcpu_count = 2;
        node.state.upgrade_state.active = false;
        assert_eq!(
            "node upgrade failed with: 'vm upgrade failed'; and then rollback failed with: 'vm rollback failed'; node ended in failed state",
            node.upgrade(new_state.clone()).await.unwrap_err().to_string()
        );
        node.state.expected_status = VmStatus::Stopped;
        node.state.upgrade_state.active = false;
        assert_eq!(
            "node upgrade failed with: 'Rhai syntax error: Cannot open script file 'new/invalid/path': No such file or directory (os error 2)'; and then rollback failed with: 'Rhai syntax error: Cannot open script file 'old/invalid/path': No such file or directory (os error 2)'; node ended in failed state",
            node.upgrade(new_state.clone()).await.unwrap_err().to_string()
        );
        node.state.expected_status = VmStatus::Stopped;
        node.state.upgrade_state.active = false;
        assert_eq!(
            "node upgrade failed with: 'BV internal error: 'firewall failure': firewall failure'; and then rollback failed with: 'BV internal error: 'firewall rollback failure': firewall rollback failure'; node ended in failed state",
            node.upgrade(new_state.clone()).await.unwrap_err().to_string()
        );

        node.state.expected_status = VmStatus::Stopped;
        node.state.upgrade_state.active = false;
        node.upgrade(new_state.clone()).await.unwrap();
        let mut state_backup: StateBackup = node_state.into();
        state_backup.initialized = true;
        new_state.upgrade_state = UpgradeState {
            active: false,
            state_backup: Some(state_backup),
            need_rollback: None,
            steps: vec![
                UpgradeStep::CpuAssignment(CpuAssignmentUpdate::AcquiredCpus(1)),
                UpgradeStep::Vm,
                UpgradeStep::Plugin,
                UpgradeStep::Firewall,
            ],
            data_stamp: None,
        };
        new_state.initialized = false;
        assert_eq!(new_state, node.state);
        assert_eq!("some_new_name", node.node_env.node_name);

        Ok(())
    }

    #[tokio::test]
    async fn test_upgrade_running_node() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let mut node_state = default_node_state();
        node_state.expected_status = VmStatus::Running;
        let mut new_state = node_state.clone();
        new_state.image.id = "new-image-id".to_string();
        let mut vm_mock = MockTestVM::new();
        let data_dir = test_env.tmp_root.clone();
        vm_mock
            .expect_data_dir()
            .returning(move || data_dir.clone());
        test_env.add_plugin_update_expectations(&mut vm_mock);

        add_firewall_expectation(&mut pal, node_state.clone());
        let test_tmp_root = test_env.tmp_root.to_path_buf();
        pal.expect_create_node_connection()
            .once()
            .returning(move |_| {
                let mut mock = MockTestNodeConnection::new();
                mock.expect_is_closed().returning(|| false);
                mock.expect_is_broken().returning(|| false);
                let tmp_root = test_tmp_root.clone();
                mock.expect_babel_client()
                    .returning(move || Ok(test_babel_client(&tmp_root)));
                mock.expect_mark_broken().return_once(|| ());
                mock.expect_engine_socket_path()
                    .return_const(Default::default());
                mock.expect_setup().returning(|| Ok(()));
                mock
            });

        // failed to stop before upgrade
        vm_mock
            .expect_state()
            .times(2)
            .returning(|| VmState::RUNNING);

        // can't start node which is not stopped properly
        vm_mock.expect_state().once().returning(|| VmState::RUNNING);
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_upgrade().once().returning(|_| Ok(()));
        test_env.add_plugin_update_expectations(&mut vm_mock);
        add_firewall_expectation(&mut pal, new_state.clone());
        vm_mock.expect_state().once().returning(|| VmState::RUNNING);
        vm_mock.expect_state().once().returning(|| VmState::RUNNING);
        vm_mock.expect_rollback().once().returning(|| Ok(()));
        test_env.add_plugin_update_expectations(&mut vm_mock);
        pal.expect_apply_firewall_config()
            .once()
            .returning(|_| Ok(()));
        vm_mock.expect_state().once().returning(|| VmState::RUNNING);

        // init failed, but data changed
        vm_mock.expect_state().once().returning(|| VmState::RUNNING);
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_upgrade().once().returning(|_| Ok(()));
        test_env.add_plugin_update_expectations(&mut vm_mock);
        add_firewall_expectation(&mut pal, new_state.clone());
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_state().once().returning(|| VmState::RUNNING);

        pal.expect_create_vm().return_once(move |_, _| Ok(vm_mock));
        let mut node = Node::create(
            Arc::new(pal),
            config,
            node_state,
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Running, node.expected_status());

        let mut babel_mock = MockTestBabelService::new();
        babel_mock
            .expect_get_jobs()
            .once()
            .returning(|_| Ok(Response::new(HashMap::default())));
        babel_mock
            .expect_get_babel_shutdown_timeout()
            .once()
            .returning(|_| Ok(Response::new(Duration::from_millis(1))));
        babel_mock
            .expect_shutdown_babel()
            .withf(|req| !req.get_ref())
            .times(4)
            .returning(|_| Err(Status::internal("can't stop babel")));
        babel_mock
            .expect_get_jobs()
            .once()
            .returning(|_| Ok(Response::new(HashMap::default())));

        babel_mock
            .expect_check_job_runner()
            .once()
            .returning(|_| Ok(Response::new(BinaryStatus::Ok)));
        babel_mock
            .expect_setup_babel()
            .once()
            .returning(|_| Ok(Response::new(())));
        babel_mock
            .expect_get_jobs()
            .once()
            .returning(|_| Ok(Response::new(HashMap::default())));
        let data_dir = test_env.tmp_root.clone();
        babel_mock.expect_run_sh().once().returning(move |_| {
            babel_api::utils::touch_protocol_data(&data_dir).unwrap();
            Err(Status::internal("can't run init command"))
        });
        fs::write(&test_env.tmp_root.join("job_runner"), "dummy job_runner")
            .await
            .unwrap();
        let server = test_env.start_server(babel_mock).await;

        assert!(node.upgrade(new_state.clone()).await.unwrap_err().to_string().starts_with("BV internal error: 'Failed to gracefully shutdown babel and background jobs: status: Internal, message: \"can't stop babel\""));
        assert!(node.state.restarting);

        node.state.expected_status = VmStatus::Running;
        assert_eq!(
            "node upgrade failed with: 'can't start node which is not stopped properly'; and then rollback failed with: 'can't start node which is not stopped properly'; node ended in failed state",
            node.upgrade(new_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );

        node.state.expected_status = VmStatus::Running;
        let error_message = node
            .upgrade(new_state.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(error_message.contains("can't run init command"));
        assert!(
            error_message.ends_with("and then rollback failed with: 'can't rollback node upgrade if 'init' was already started - protocol data could be changed'; node ended in failed state")
        );

        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_upgrade_rejected() -> Result<()> {
        let test_env = TestEnv::new().await?;
        let mut pal = test_env.default_pal();
        let config = default_config(test_env.tmp_root.clone());
        let node_state = default_node_state();
        let mut vm_mock = MockTestVM::new();
        test_env.add_plugin_update_expectations(&mut vm_mock);
        vm_mock.expect_state().once().returning(|| VmState::SHUTOFF);
        vm_mock.expect_state().once().returning(|| VmState::RUNNING);
        let data_dir = test_env.tmp_root.clone();
        vm_mock
            .expect_data_dir()
            .returning(move || data_dir.clone());
        test_env.add_create_node_expectations(&mut pal, node_state.clone(), vm_mock);

        let mut node = Node::create(
            Arc::new(pal),
            config,
            node_state.clone(),
            test_env.tx.clone(),
            default_cpu_registry(),
        )
        .await?;
        assert_eq!(VmStatus::Running, node.expected_status());

        let mut babel_mock = MockTestBabelService::new();
        // failed to gracefully shutdown babel - upgrade blocking job
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

        assert_eq!(
            "BV internal error: 'can't upgrade node in Failed state'",
            node.upgrade(node_state.clone())
                .await
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "can't proceed while 'upgrade_blocking' job is running. Try again after 3600 seconds.",
            node.upgrade(node_state).await.unwrap_err().to_string()
        );

        server.assert().await;
        Ok(())
    }
}
