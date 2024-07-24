use crate::{
    api_with_retry, cluster,
    config::{Config, SharedConfig},
    hosts::{self, HostMetrics},
    internal_server, node_metrics,
    node_state::NodeStatus,
    nodes_manager::NodesManager,
    pal::{CommandsStream, Pal, ServiceConnector},
    self_updater,
    services::{
        self,
        api::{self, common, pb, MetricsServiceClient},
        mqtt, ApiClient, ApiInterceptor, ApiServiceConnector,
    },
    try_set_bv_status,
    utils::with_jitter,
    ServiceStatus,
};
use bv_utils::run_flag::RunFlag;
use eyre::{Context, Result};
use metrics::{register_counter, Counter};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt::Debug,
    hash::{Hash, Hasher},
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};
use tokio::{
    net::TcpListener,
    sync::watch::Sender,
    sync::{watch, watch::Receiver},
    time::{sleep, Duration},
};
use tonic::transport::{Channel, Server};
use tracing::{debug, error, info, warn};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_CHECK_INTERVAL: Duration = Duration::from_secs(15);
const INFO_UPDATE_INTERVAL: Duration = Duration::from_secs(30);
const CLUSTER_UPDATES_INTERVAL: Duration = Duration::from_secs(30);

lazy_static::lazy_static! {
    pub static ref BV_HOST_METRICS_COUNTER: Counter = register_counter!("bv.periodic.host.metrics.calls");
    pub static ref BV_HOST_METRICS_TIME_MS_COUNTER: Counter = register_counter!("bv.periodic.host.metrics.ms");
    pub static ref BV_NODES_RECOVERY_COUNTER: Counter = register_counter!("bv.periodic.nodes.recovery.calls");
    pub static ref BV_NODES_RECOVERY_TIME_MS_COUNTER: Counter = register_counter!("bv.periodic.nodes.recovery.ms");
    pub static ref BV_NODES_METRICS_COUNTER: Counter = register_counter!("bv.periodic.nodes.metrics.calls");
    pub static ref BV_NODES_METRICS_TIME_MS_COUNTER: Counter = register_counter!("bv.periodic.nodes.metrics.ms");
    pub static ref BV_NODES_INFO_COUNTER: Counter = register_counter!("bv.periodic.nodes.info.calls");
    pub static ref BV_NODES_INFO_TIME_MS_COUNTER: Counter = register_counter!("bv.periodic.nodes.info.ms");
    pub static ref BV_CLUSTER_UPDATES_COUNTER: Counter = register_counter!("bv.periodic.cluster.updates.calls");
    pub static ref BV_CLUSTER_UPDATES_TIME_MS_COUNTER: Counter = register_counter!("bv.periodic.cluster.updates.ms");
}

pub struct BlockvisorD<P> {
    pal: P,
    config: SharedConfig,
    listener: TcpListener,
    cluster: Arc<Option<cluster::ClusterData>>,
}

impl<P> BlockvisorD<P>
where
    P: Pal + Send + Sync + Debug + 'static,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    pub async fn new(pal: P, config: Config) -> Result<Self> {
        let bv_root = pal.bv_root().to_owned();
        let url = format!("0.0.0.0:{}", config.blockvisor_port);
        let listener = TcpListener::bind(url).await?;
        let maybe_cluster = cluster::start_server(&config).await?;

        Ok(Self {
            pal,
            config: SharedConfig::new(config, bv_root),
            listener,
            cluster: Arc::new(maybe_cluster),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn run(self, mut run: RunFlag) -> Result<()> {
        let bv_root = self.pal.bv_root().to_path_buf();
        let config = self.config.read().await;

        // check for updates on start
        // this could enable us to update bv that has a critical bug and is crashing on start
        let mut self_updater =
            self_updater::new(bv_utils::timer::SysTimer, &bv_root, &self.config).await?;
        if config.update_check_interval_secs.is_some() {
            if let Err(e) = self_updater.check_for_update().await {
                warn!("Cannot run self update: {e:#}");
            }
        }
        let self_updater_future = self_updater.run(run.clone());

        let cmds_connector = self.pal.create_commands_stream_connector(&self.config);
        let nodes_manager = NodesManager::load(self.pal, self.config.clone()).await?;
        let nodes_manager = Arc::new(nodes_manager);

        try_set_bv_status(ServiceStatus::Ok).await;

        let internal_api_server_future = Self::create_internal_api_server(
            self.config.clone(),
            run.clone(),
            self.listener,
            nodes_manager.clone(),
            self.cluster.clone(),
        );

        let (cmd_watch_tx, cmd_watch_rx) = watch::channel(());
        let external_api_client_future = Self::create_external_api_listener(
            run.clone(),
            nodes_manager.clone(),
            cmd_watch_rx,
            &self.config,
        );
        let mqtt_notification_future =
            Self::create_commands_listener(run.clone(), cmds_connector, cmd_watch_tx);

        let cluster_updates_future =
            Self::cluster_updates(run.clone(), nodes_manager.clone(), self.cluster.clone());

        let nodes_recovery_future = Self::nodes_recovery(run.clone(), nodes_manager.clone());

        let node_updates_future =
            Self::node_updates(run.clone(), nodes_manager.clone(), self.config.clone());

        let node_metrics_future =
            Self::node_metrics(run.clone(), nodes_manager.clone(), self.config.clone());
        let host_metrics_future = Self::host_metrics(
            run.clone(),
            nodes_manager.clone(),
            config.id,
            self.config.clone(),
        );

        // send up to date information about host software
        if let Err(e) = hosts::send_info_update(self.config.clone()).await {
            warn!("Cannot send host info update: {e:#}");
        }
        let _ = tokio::join!(
            internal_api_server_future,
            external_api_client_future,
            mqtt_notification_future,
            cluster_updates_future,
            nodes_recovery_future,
            node_updates_future,
            node_metrics_future,
            host_metrics_future,
            self_updater_future
        );
        info!("Stopping...");
        Arc::into_inner(nodes_manager).unwrap().detach().await;
        self.config.read().await.save(&self.config.bv_root).await?;
        Ok(())
    }

    async fn create_internal_api_server(
        config: SharedConfig,
        mut run: RunFlag,
        listener: TcpListener,
        nodes_manager: Arc<NodesManager<P>>,
        cluster: Arc<Option<cluster::ClusterData>>,
    ) -> Result<()> {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(internal_server::service_server::ServiceServer::new(
                internal_server::State {
                    config,
                    nodes_manager,
                    cluster,
                    dev_mode: false,
                },
            ))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                run.wait(),
            )
            .await?;

        Ok(())
    }

    async fn create_external_api_listener(
        mut run: RunFlag,
        nodes_manager: Arc<NodesManager<P>>,
        mut cmd_watch_rx: Receiver<()>,
        config: &SharedConfig,
    ) {
        while run.load() {
            run.select(cmd_watch_rx.changed()).await;
            debug!("MQTT watch triggerred");
            api::get_and_process_pending_commands(config, nodes_manager.clone()).await;
        }
    }

    async fn create_commands_listener(
        mut run: RunFlag,
        connector: P::CommandsStreamConnector,
        cmd_watch_tx: Sender<()>,
    ) {
        let notify = || {
            debug!("MQTT send notification");
            mqtt::MQTT_NOTIFY_COUNTER.increment(1);
            cmd_watch_tx
                .send(())
                .unwrap_or_else(|_| error!("MQTT command watch error"));
        };
        while run.load() {
            debug!("Connecting to MQTT");
            match connector.connect().await {
                Ok(mut client) => {
                    // get pending commands on reconnect
                    notify();
                    while run.load() {
                        debug!("MQTT watch wait...");
                        if let Some(cmds) = run.select(client.wait_for_pending_commands()).await {
                            match cmds {
                                Ok(Some(_)) => notify(),
                                Ok(None) => {}
                                Err(e) => {
                                    warn!("{e:#}");
                                    mqtt::MQTT_ERROR_COUNTER.increment(1);
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => warn!("Error connecting to MQTT: {e:#}"),
            }
            run.select(sleep(RECONNECT_INTERVAL)).await;
            // get pending commands if mqtt is not avail
            notify();
        }
    }

    /// This task runs periodically to update nodes info in p2p network.
    async fn cluster_updates(
        mut run: RunFlag,
        nodes_manager: Arc<NodesManager<P>>,
        cluster: Arc<Option<cluster::ClusterData>>,
    ) {
        while run.load() {
            let now = Instant::now();
            if let Some(ref cluster) = *cluster {
                // collect interesting information about nodes
                let mut updates = vec![];
                for (id, node) in nodes_manager.nodes_list().await.iter() {
                    if let Ok(node) = node.try_read() {
                        let status = node.status().await;
                        let image = node.state.image.clone();
                        updates.push((node.id(), status, image));
                    } else {
                        debug!(
                            "Skipping node info collection for cluster metadata, node `{id}` busy"
                        );
                    }
                }
                // just render into string for now
                match serde_json::to_string(&updates) {
                    Ok(current_info) => {
                        let mut cluster_lock = cluster.chitchat.lock().await;
                        let host_state = cluster_lock.self_node_state();
                        // update cluster state if old value differs from new
                        match host_state.get("nodes") {
                            Some(info) if info == current_info => {}
                            _ => {
                                host_state.set("nodes", current_info);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Cannot serialize node updates for cluster: {e:#}");
                    }
                }
            };
            BV_CLUSTER_UPDATES_COUNTER.increment(1);
            BV_CLUSTER_UPDATES_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            run.select(sleep(CLUSTER_UPDATES_INTERVAL)).await;
        }
    }
    /// This task runs periodically to make sure actual nodes state is equal to expected.
    async fn nodes_recovery(mut run: RunFlag, nodes_manager: Arc<NodesManager<P>>) {
        while run.load() {
            let now = Instant::now();
            nodes_manager.recover().await;
            BV_NODES_RECOVERY_COUNTER.increment(1);
            BV_NODES_RECOVERY_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
            run.select(sleep(RECOVERY_CHECK_INTERVAL)).await;
        }
    }

    /// This task runs periodically to send important info about nodes to API.
    async fn node_updates(
        mut run: RunFlag,
        nodes_manager: Arc<NodesManager<P>>,
        config: SharedConfig,
    ) {
        let mut updates_cache = HashMap::new();
        while run.load() {
            run.select(sleep(with_jitter(INFO_UPDATE_INTERVAL))).await;

            let now = Instant::now();
            let mut updates = vec![];
            for (id, node) in nodes_manager.nodes_list().await.iter() {
                if let Ok(mut node) = node.try_write() {
                    if node.state.dev_mode {
                        // don't send updates for nodes in dev mode
                        continue;
                    }
                    let status = node.status().await;
                    let maybe_address = if status == NodeStatus::Running
                        && node.babel_engine.has_capability("address")
                    {
                        node.babel_engine.address().await.ok()
                    } else {
                        None
                    };
                    debug!("Collected node `{id}` info: s={status}, a={maybe_address:?}");
                    updates.push((node.id(), status, maybe_address));
                } else {
                    debug!("Skipping node info collection, node `{id}` busy");
                }
            }

            for (node_id, status, address) in updates {
                let container_status = match status {
                    NodeStatus::Running => common::ContainerStatus::Running,
                    NodeStatus::Stopped => common::ContainerStatus::Stopped,
                    NodeStatus::Failed => common::ContainerStatus::Failed,
                    NodeStatus::Busy => common::ContainerStatus::Busy,
                };
                let mut update = pb::NodeServiceUpdateStatusRequest {
                    id: node_id.to_string(),
                    container_status: None, // We use the setter to set this field for type-safety
                    address,
                    version: None,
                };
                update.set_container_status(container_status);

                if updates_cache.get(&node_id) == Some(&update) {
                    debug!("Skipping node update: {update:?}");
                    continue;
                }

                match Self::send_node_status_update(&config, update.clone()).await {
                    Ok(_) => {
                        // cache to not send the same data if it has not changed
                        updates_cache.insert(node_id, update);
                    }
                    Err(e) => warn!("Cannot send node `{node_id}` info update: {e:#}"),
                }
            }
            BV_NODES_INFO_COUNTER.increment(1);
            BV_NODES_INFO_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
        }
    }

    /// This task runs periodically to aggregate metrics from every node. It will call into the
    /// nodes, query their metrics, then send them to blockvisor-api.
    async fn node_metrics(
        mut run: RunFlag,
        nodes_manager: Arc<NodesManager<P>>,
        config: SharedConfig,
    ) -> Option<()> {
        let mut job_info_cache: HashMap<(uuid::Uuid, String), u64> = HashMap::new();
        while run.load() {
            run.select(sleep(with_jitter(node_metrics::COLLECT_INTERVAL)))
                .await;
            let now = Instant::now();
            let mut metrics = node_metrics::collect_metrics(nodes_manager.clone()).await;
            let mut job_info_cache_update: HashMap<(uuid::Uuid, String), u64> = HashMap::new();
            // do not bother api with empty updates
            if metrics.has_any() {
                if let Ok(mut client) = Self::connect_metrics_service(&config).await {
                    for (id, metric) in metrics.iter_mut() {
                        // go through all jobs info in metrics and leave only this that has changed
                        // since last time
                        metric.jobs.retain(|name, info| {
                            let mut state = DefaultHasher::new();
                            info.hash(&mut state);
                            let new_hash = state.finish();
                            let key = (*id, name.clone());
                            if Some(&new_hash) != job_info_cache.get(&key) {
                                job_info_cache_update.insert(key, new_hash);
                                debug!(
                                    "job info for '{name}' on {id} has changed - adding to metrics"
                                );
                                true
                            } else {
                                false
                            }
                        });
                    }
                    let metrics: pb::MetricsServiceNodeRequest = metrics.into();
                    if let Err(err) = api_with_retry!(client, client.node(metrics.clone())) {
                        warn!("Could not send node metrics! `{err:#}`");
                    } else {
                        // update cache only if request succeed
                        job_info_cache.extend(job_info_cache_update.into_iter());
                    }
                    BV_NODES_METRICS_COUNTER.increment(1);
                    BV_NODES_METRICS_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
                }
            } else {
                info!("No metrics collected");
            }
        }
        None
    }

    async fn host_metrics(
        mut run: RunFlag,
        nodes_manager: Arc<NodesManager<P>>,
        host_id: String,
        config: SharedConfig,
    ) -> Option<()> {
        while run.load() {
            run.select(sleep(with_jitter(hosts::COLLECT_INTERVAL)))
                .await;

            let now = Instant::now();
            match HostMetrics::collect(nodes_manager.nodes_data_cache().await, nodes_manager.pal())
            {
                Ok(metrics) => {
                    if let Ok(mut client) = Self::connect_metrics_service(&config).await {
                        metrics.set_all_gauges();
                        let metrics = pb::MetricsServiceHostRequest::new(host_id.clone(), metrics);
                        if let Err(err) = api_with_retry!(client, client.host(metrics.clone())) {
                            warn!("Could not send host metrics! `{err:#}`");
                        }
                    }
                }
                Err(err) => {
                    warn!("Could not collect host metrics! `{err:#}`");
                }
            };
            BV_HOST_METRICS_COUNTER.increment(1);
            BV_HOST_METRICS_TIME_MS_COUNTER.increment(now.elapsed().as_millis() as u64);
        }
        None
    }

    async fn connect_metrics_service(
        config: &SharedConfig,
    ) -> Result<
        ApiClient<
            MetricsServiceClient,
            impl ApiServiceConnector,
            impl Fn(Channel, ApiInterceptor) -> MetricsServiceClient + Clone,
        >,
        tonic::Status,
    > {
        let client = services::ApiClient::build_with_default_connector(
            config,
            pb::metrics_service_client::MetricsServiceClient::with_interceptor,
        )
        .await;
        if let Err(err) = &client {
            warn!("Metrics could not be sent: {err:#}");
        };
        client
    }

    // Send node info update to control plane
    async fn send_node_status_update(
        config: &SharedConfig,
        update: pb::NodeServiceUpdateStatusRequest,
    ) -> Result<()> {
        let mut client = services::ApiClient::build_with_default_connector(
            config,
            pb::node_service_client::NodeServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "Error connecting to api".to_string())?;
        api_with_retry!(client, client.update_status(update.clone()))
            .with_context(|| "Cannot send node update".to_string())?;
        Ok(())
    }
}
