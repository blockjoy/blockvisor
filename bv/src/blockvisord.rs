use crate::{
    config::{Config, CONFIG_PATH},
    hosts,
    node_data::NodeStatus,
    node_metrics,
    nodes::Nodes,
    pal::{NetInterface, Pal},
    self_updater,
    server::{bv_pb, BlockvisorServer},
    services::{api, api::pb},
    try_set_bv_status,
};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fmt::Debug;
use std::{net::ToSocketAddrs, str::FromStr, sync::Arc};
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};
use tonic::transport::{Channel, Endpoint, Server};
use tracing::{error, info, warn};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const INFO_UPDATE_INTERVAL: Duration = Duration::from_secs(30);

pub struct BlockvisorD<P> {
    pub pal: P,
}

impl<P> BlockvisorD<P>
where
    P: Pal + Send + Sync + Debug + 'static,
    <P as Pal>::NetInterface: Send + Sync + Clone,
{
    pub async fn run(self) -> Result<()> {
        let bv_root = self.pal.bv_root().to_path_buf();
        let config = Config::load(&bv_root).await.with_context(|| {
            format!(
                "failed to load host config from {}",
                bv_root.join(CONFIG_PATH).display()
            )
        })?;
        let nodes = if Nodes::<P>::exists(&bv_root) {
            Nodes::load(self.pal, config.clone()).await?
        } else {
            let nodes = Nodes::new(self.pal, config.clone());
            nodes.save().await?;
            nodes
        };

        try_set_bv_status(bv_pb::ServiceStatus::Ok).await;
        let nodes = Arc::new(RwLock::new(nodes));

        let url = format!("0.0.0.0:{}", config.blockvisor_port);
        let server = BlockvisorServer {
            nodes: nodes.clone(),
        };
        let internal_api_server_future = Self::create_server(url, server);

        let token = api::AuthToken(config.token.to_owned());
        let endpoint = Endpoint::from_str(&config.blockjoy_api_url)?;
        let external_api_client_future = async {
            loop {
                match api::CommandsService::connect(&config.blockjoy_api_url, &config.token).await {
                    Ok(mut client) => {
                        if let Err(e) = client
                            .get_and_process_pending_commands(&config.id, nodes.clone())
                            .await
                        {
                            error!("Error processing pending commands: {:?}", e);
                        }
                    }
                    Err(e) => error!("Error connecting to api: {:?}", e),
                }

                sleep(RECONNECT_INTERVAL).await;
            }
        };

        let nodes_recovery_future = async {
            loop {
                let list: Vec<_> = nodes
                    .read()
                    .await
                    .list()
                    .await
                    .iter()
                    .map(|node| (node.data.clone(), node.status()))
                    .collect();

                for (node_data, node_status) in list {
                    let id = node_data.id;
                    if node_status == NodeStatus::Failed {
                        match node_data.expected_status {
                            NodeStatus::Running => {
                                info!("Recovery: starting node with ID `{id}`");
                                if let Err(e) = node_data.network_interface.remaster().await {
                                    error!("Recovery: remastering network for node with ID `{id}` failed: {e}");
                                }
                                if let Err(e) = nodes.write().await.force_start(id).await {
                                    error!("Recovery: starting node with ID `{id}` failed: {e}");
                                }
                            }
                            NodeStatus::Stopped => {
                                info!("Recovery: stopping node with ID `{id}`");
                                if let Err(e) = nodes.write().await.force_stop(id).await {
                                    error!("Recovery: stopping node with ID `{id}` failed: {e}",);
                                }
                            }
                            NodeStatus::Failed => {
                                warn!("Recovery: node with ID `{id}` cannot be recovered");
                            }
                        }
                    }
                }
                sleep(RECOVERY_CHECK_INTERVAL).await;
            }
        };

        let node_updates_future = Self::node_updates(nodes.clone());
        let node_metrics_future = Self::node_metrics(nodes.clone(), &endpoint, token.clone());
        let host_metrics_future = Self::host_metrics(config.id.clone(), &endpoint, token.clone());
        let self_updater = self_updater::new(self_updater::SysTimer, &bv_root, &config)?;

        let _ = tokio::join!(
            internal_api_server_future,
            external_api_client_future,
            nodes_recovery_future,
            node_updates_future,
            node_metrics_future,
            host_metrics_future,
            self_updater.run()
        );
        Ok(())
    }

    async fn create_server(url: String, server: BlockvisorServer<P>) -> Result<()> {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(bv_pb::blockvisor_server::BlockvisorServer::new(server))
            .serve(url.to_socket_addrs()?.next().unwrap())
            .await?;

        Ok(())
    }

    async fn wait_for_channel(endpoint: &Endpoint) -> Channel {
        loop {
            match Endpoint::connect(endpoint).await {
                Ok(channel) => return channel,
                Err(e) => {
                    error!("Error connecting to endpoint: {:?}", e);
                    sleep(RECONNECT_INTERVAL).await;
                }
            }
        }
    }

    /// This task runs periodically to send important info about nodes to API.
    async fn node_updates(nodes: Arc<RwLock<Nodes<P>>>) {
        let mut timer = tokio::time::interval(INFO_UPDATE_INTERVAL);
        let mut known_addresses: HashMap<String, String> = HashMap::new();
        loop {
            timer.tick().await;
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

    /// This task runs every minute to aggregate metrics from every node. It will call into the nodes
    /// query their metrics, then send them to blockvisor-api.
    async fn node_metrics(
        nodes: Arc<RwLock<Nodes<P>>>,
        endpoint: &Endpoint,
        token: api::AuthToken,
    ) {
        let mut timer = tokio::time::interval(node_metrics::COLLECT_INTERVAL);
        loop {
            timer.tick().await;
            let mut lock = nodes.write().await;
            let metrics = node_metrics::collect_metrics(lock.nodes.values_mut()).await;
            // Drop the lock as early as possible.
            drop(lock);
            let mut client = api::MetricsClient::with_auth(
                Self::wait_for_channel(endpoint).await,
                token.clone(),
            );
            let metrics: pb::NodeMetricsRequest = metrics.into();
            if let Err(e) = client.node(metrics).await {
                error!("Could not send node metrics! `{e}`");
            }
        }
    }

    async fn host_metrics(host_id: String, endpoint: &Endpoint, token: api::AuthToken) {
        let mut timer = tokio::time::interval(hosts::COLLECT_INTERVAL);
        loop {
            timer.tick().await;
            match hosts::get_host_metrics() {
                Ok(metrics) => {
                    let mut client = api::MetricsClient::with_auth(
                        Self::wait_for_channel(endpoint).await,
                        token.clone(),
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
    }
}
