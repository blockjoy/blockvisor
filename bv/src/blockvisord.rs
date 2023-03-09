use crate::{
    config::{Config, SharedConfig, CONFIG_PATH},
    hosts,
    node_data::NodeStatus,
    node_metrics,
    nodes::Nodes,
    pal::{CommandsStream, Pal, ServiceConnector},
    self_updater,
    server::{bv_pb, BlockvisorServer},
    services::{api, api::pb},
    try_set_bv_status,
};
use anyhow::{Context, Result};
use bv_utils::run_flag::RunFlag;
use std::{collections::HashMap, fmt::Debug, net::SocketAddr, str::FromStr, sync::Arc};
use tokio::sync::watch::Sender;
use tokio::{
    net::TcpListener,
    sync::{watch, watch::Receiver, RwLock},
    time::{sleep, Duration},
};
use tonic::transport::{Channel, Endpoint, Server};
use tracing::{debug, error, info, warn};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const INFO_UPDATE_INTERVAL: Duration = Duration::from_secs(30);

pub struct BlockvisorD<P> {
    pal: P,
    config: SharedConfig,
    listener: TcpListener,
}

impl<P> BlockvisorD<P>
where
    P: Pal + Send + Sync + Debug + 'static,
    <P as Pal>::NetInterface: Send + Sync + Clone,
{
    pub async fn new(pal: P) -> Result<Self> {
        let bv_root = pal.bv_root();
        let config = Config::load(bv_root).await.with_context(|| {
            format!(
                "failed to load host config from {}",
                bv_root.join(CONFIG_PATH).display()
            )
        })?;
        let url = format!("0.0.0.0:{}", config.blockvisor_port);
        let listener = TcpListener::bind(url).await?;
        Ok(Self {
            pal,
            config: SharedConfig::new(config),
            listener,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn run(self, run: RunFlag) -> Result<()> {
        info!(
            "Starting {} {} ...",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );

        let bv_root = self.pal.bv_root().to_path_buf();
        let cmds_connector = self.pal.create_commands_stream_connector(&self.config);
        let nodes = Nodes::load(self.pal, self.config.clone()).await?;

        try_set_bv_status(bv_pb::ServiceStatus::Ok).await;
        let nodes = Arc::new(RwLock::new(nodes));

        let server = BlockvisorServer {
            nodes: nodes.clone(),
        };
        let internal_api_server_future =
            Self::create_internal_api_server(run.clone(), self.listener, server);

        let (cmd_watch_tx, cmd_watch_rx) = watch::channel(());
        let external_api_client_future = Self::create_external_api_listener(
            run.clone(),
            nodes.clone(),
            cmd_watch_rx,
            &self.config,
        );
        let mqtt_notification_future =
            Self::create_commands_listener(run.clone(), cmds_connector, cmd_watch_tx);

        let nodes_recovery_future = Self::nodes_recovery(run.clone(), nodes.clone());

        let node_updates_future = Self::node_updates(run.clone(), nodes.clone());

        let config = self.config.read().await;
        let endpoint = Endpoint::from_str(&config.blockjoy_api_url)?;
        let node_metrics_future =
            Self::node_metrics(run.clone(), nodes.clone(), &endpoint, self.config.clone());
        let host_metrics_future =
            Self::host_metrics(run.clone(), config.id, &endpoint, self.config.clone());

        let self_updater_future = self_updater::new(self_updater::SysTimer, &bv_root, &self.config)
            .await?
            .run();

        let _ = tokio::join!(
            internal_api_server_future,
            external_api_client_future,
            mqtt_notification_future,
            nodes_recovery_future,
            node_updates_future,
            node_metrics_future,
            host_metrics_future,
            self_updater_future
        );
        info!("Stopping...");
        // store fresh config before exit since service urls could change
        self.config.read().await.save(&bv_root).await?;
        Ok(())
    }

    async fn create_internal_api_server(
        mut run: RunFlag,
        listener: TcpListener,
        server: BlockvisorServer<P>,
    ) -> Result<()> {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(bv_pb::blockvisor_server::BlockvisorServer::new(server))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                run.wait(),
            )
            .await?;

        Ok(())
    }

    async fn create_external_api_listener(
        mut run: RunFlag,
        nodes: Arc<RwLock<Nodes<P>>>,
        mut cmd_watch_rx: Receiver<()>,
        config: &SharedConfig,
    ) {
        while run.load() {
            tokio::select! {
                _ = cmd_watch_rx.changed() => {
                    debug!("MQTT watch triggerred");
                    match api::CommandsService::connect(config.read().await).await {
                        Ok(mut client) => {
                            if let Err(e) = client.get_and_process_pending_commands(&config.read().await.id, nodes.clone()).await {
                                error!("Error processing pending commands: {:?}", e);
                            }
                        }
                        Err(e) => warn!("Error connecting to api: {:?}", e),
                    }
                }
                _ = sleep(RECONNECT_INTERVAL) => {
                    debug!("Waiting for commands notification...");
                }
                _ = run.wait() => {}
            }
        }
    }

    async fn create_commands_listener(
        mut run: RunFlag,
        connector: P::CommandsStreamConnector,
        cmd_watch_tx: Sender<()>,
    ) {
        let notify = || {
            debug!("MQTT send notification");
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
                        tokio::select! {
                            cmds = client.wait_for_pending_commands() => {
                                match cmds {
                                    Ok(Some(_)) => notify(),
                                    Ok(None) => {}
                                    Err(e) => {
                                        warn!("MQTT error: {e:?}");
                                        break;
                                    }
                                }
                            }
                            _ = run.wait() => {}
                        }
                    }
                }
                Err(e) => warn!("Error connecting to MQTT: {:?}", e),
            }
            run.select(sleep(RECONNECT_INTERVAL)).await;
            // get pending commands if mqtt is not avail
            notify();
        }
    }

    async fn nodes_recovery(mut run: RunFlag, nodes: Arc<RwLock<Nodes<P>>>) {
        while run.load() {
            let list: Vec<_> = nodes
                .read()
                .await
                .list()
                .await
                .iter()
                .map(|node| (node.data.id, node.status(), node.expected_status()))
                .collect();

            for (id, node_status, expected_status) in list {
                if node_status == NodeStatus::Failed && expected_status != NodeStatus::Failed {
                    if let Err(e) = nodes.write().await.recover(id).await {
                        error!("Recovery: node with ID `{id}` failed: {e}");
                    }
                }
            }
            run.select(sleep(RECOVERY_CHECK_INTERVAL)).await;
        }
    }

    /// This task runs periodically to send important info about nodes to API.
    async fn node_updates(mut run: RunFlag, nodes: Arc<RwLock<Nodes<P>>>) {
        let mut timer = tokio::time::interval(INFO_UPDATE_INTERVAL);
        let mut known_addresses: HashMap<String, String> = HashMap::new();
        while run.load() {
            run.select(timer.tick()).await;
            let mut nodes_lock = nodes.write().await;

            let mut updates = vec![];
            for node in nodes_lock.nodes.values_mut() {
                if let Ok(address) = node.address().await {
                    updates.push((node.id().to_string(), address));
                }
            }

            for (node_id, address) in updates {
                if known_addresses.get(&node_id) == Some(&address) {
                    continue;
                }

                let update = pb::NodeInfo {
                    id: node_id.clone(),
                    address: Some(address.clone()),
                    ..Default::default()
                };

                if nodes_lock.send_info_update(update).await.is_ok() {
                    // cache addresses to not send the same address if it has not changed
                    known_addresses.entry(node_id).or_insert(address);
                }
            }
        }
    }

    async fn wait_for_channel(mut run: RunFlag, endpoint: &Endpoint) -> Option<Channel> {
        while run.load() {
            match Endpoint::connect(endpoint).await {
                Ok(channel) => return Some(channel),
                Err(e) => {
                    warn!("Error connecting to endpoint: {:?}", e);
                    run.select(sleep(RECONNECT_INTERVAL)).await;
                }
            }
        }
        None
    }

    /// This task runs every minute to aggregate metrics from every node. It will call into the nodes
    /// query their metrics, then send them to blockvisor-api.
    async fn node_metrics(
        mut run: RunFlag,
        nodes: Arc<RwLock<Nodes<P>>>,
        endpoint: &Endpoint,
        config: SharedConfig,
    ) -> Option<()> {
        let mut timer = tokio::time::interval(node_metrics::COLLECT_INTERVAL);
        while run.load() {
            run.select(timer.tick()).await;
            let mut lock = nodes.write().await;
            let metrics = node_metrics::collect_metrics(lock.nodes.values_mut()).await;
            // Drop the lock as early as possible.
            drop(lock);
            let mut client = api::MetricsClient::with_auth(
                Self::wait_for_channel(run.clone(), endpoint).await?,
                api::AuthToken(config.read().await.token),
            );
            let metrics: pb::NodeMetricsRequest = metrics.into();
            if let Err(e) = client.node(metrics).await {
                error!("Could not send node metrics! `{e}`");
            }
        }
        None
    }

    async fn host_metrics(
        mut run: RunFlag,
        host_id: String,
        endpoint: &Endpoint,
        config: SharedConfig,
    ) -> Option<()> {
        let mut timer = tokio::time::interval(hosts::COLLECT_INTERVAL);
        while run.load() {
            run.select(timer.tick()).await;
            match hosts::get_host_metrics() {
                Ok(metrics) => {
                    let mut client = api::MetricsClient::with_auth(
                        Self::wait_for_channel(run.clone(), endpoint).await?,
                        api::AuthToken(config.read().await.token),
                    );
                    let metrics = pb::HostMetricsRequest::new(host_id.clone(), metrics);
                    if let Err(e) = client.host(metrics).await {
                        error!("Could not send host metrics! `{e}`");
                    }
                }
                Err(e) => {
                    error!("Could not collect host metrics! `{e}`");
                }
            };
        }
        None
    }
}
