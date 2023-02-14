use crate::api::pb;
use anyhow::{Context, Result};
use blockvisord::pal::{LinuxPlatform, NetInterface};
use blockvisord::{
    config::Config,
    hosts,
    logging::setup_logging,
    node_data::NodeStatus,
    node_metrics,
    nodes::Nodes,
    self_updater,
    server::{bv_pb, BlockvisorServer, BLOCKVISOR_SERVICE_PORT},
    services::api,
    try_set_bv_status,
};
use std::collections::HashMap;
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

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let config = Config::load().await.context("failed to load host config")?;
    let nodes = if Nodes::<LinuxPlatform>::exists() {
        Nodes::load(LinuxPlatform, config.clone()).await?
    } else {
        let nodes = Nodes::new(LinuxPlatform, config.clone());
        nodes.save().await?;
        nodes
    };

    try_set_bv_status(bv_pb::ServiceStatus::Ok).await;
    let updates_tx = nodes.get_updates_sender().await?.clone();
    let nodes = Arc::new(RwLock::new(nodes));

    let url = format!("0.0.0.0:{BLOCKVISOR_SERVICE_PORT}");
    let server = BlockvisorServer {
        nodes: nodes.clone(),
    };
    let internal_api_server_future = create_server(url, server);

    let token = api::AuthToken(config.token.to_owned());
    let endpoint = Endpoint::from_str(&config.blockjoy_api_url)?;
    let external_api_client_future = async {
        loop {
            info!("Creating gRPC client...");
            let mut client =
                api::CommandsClient::with_auth(wait_for_channel(&endpoint).await, token.clone());
            if let Err(e) =
                api::process_commands_stream(&mut client, nodes.clone(), updates_tx.clone()).await
            {
                error!("Error processing pending commands: {:?}", e);
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

    let node_updates_future = node_updates(nodes.clone());
    let node_metrics_future = node_metrics(nodes.clone(), &endpoint, token.clone());
    let host_metrics_future = host_metrics(config.id.clone(), &endpoint, token.clone());
    let self_updater = self_updater::new(self_updater::SysTimer, &config)?;

    let _ = tokio::join!(
        internal_api_server_future,
        external_api_client_future,
        nodes_recovery_future,
        node_updates_future,
        node_metrics_future,
        host_metrics_future,
        self_updater.run()
    );

    info!("Stopping...");
    Ok(())
}

async fn create_server(url: String, server: BlockvisorServer<LinuxPlatform>) -> Result<()> {
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
async fn node_updates(nodes: Arc<RwLock<Nodes<LinuxPlatform>>>) {
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

            if nodes_lock.send_info_update(update).is_ok() {
                // cache addresses to not send the same address if it has not changed
                known_addresses.entry(node_id).or_insert(address);
            }
        }
    }
}

/// This task runs every minute to aggregate metrics from every node. It will call into the nodes
/// query their metrics, then send them to blockvisor-api.
async fn node_metrics(
    nodes: Arc<RwLock<Nodes<LinuxPlatform>>>,
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
        let mut client =
            api::MetricsClient::with_auth(wait_for_channel(endpoint).await, token.clone());
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
                let mut client =
                    api::MetricsClient::with_auth(wait_for_channel(endpoint).await, token.clone());
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
