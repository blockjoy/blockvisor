use crate::api::pb;
use anyhow::{Context, Result};
use blockvisord::nodes::CommonData;
use blockvisord::self_updater::SelfUpdater;
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
use std::{net::ToSocketAddrs, str::FromStr, sync::Arc};
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};
use tonic::transport::{Channel, Endpoint, Server};
use tracing::{error, info, warn};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_CHECK_INTERVAL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    info!("Starting...");

    let config = Config::load().await.context("failed to load host config")?;
    let nodes = if Nodes::exists() {
        Nodes::load(config.clone()).await?
    } else {
        let nodes_data = CommonData { machine_index: 0 };
        let nodes = Nodes::new(config.clone(), nodes_data);
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
        let channel = wait_for_channel(&endpoint).await;

        info!("Creating gRPC client...");
        let mut client = api::CommandsClient::with_auth(channel, token.clone());

        loop {
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
            let list = nodes.read().await.list().await;

            for node in list {
                let id = &node.id;
                if node.status() == NodeStatus::Failed {
                    match node.expected_status {
                        NodeStatus::Running => {
                            info!("Recovery: starting node with ID `{id}`");
                            if let Err(e) = nodes.write().await.start(node.id).await {
                                error!("Recovery: starting node with ID `{id}` failed: {e}");
                            }
                        }
                        NodeStatus::Stopped => {
                            info!("Recovery: stopping node with ID `{id}`");
                            if let Err(e) = nodes.write().await.stop(node.id).await {
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

    let node_metrics_future = node_metrics(nodes.clone(), &endpoint, token.clone());
    let host_metrics_future = host_metrics(config.id.clone(), &endpoint, token.clone());
    let self_updater = SelfUpdater::<self_updater::SysTimer>::new(&config)?;

    let _ = tokio::join!(
        internal_api_server_future,
        external_api_client_future,
        nodes_recovery_future,
        node_metrics_future,
        host_metrics_future,
        self_updater.run()
    );

    info!("Stopping...");
    Ok(())
}

async fn create_server(url: String, server: BlockvisorServer) -> Result<()> {
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

/// This task runs every minute to aggregate metrics from every node. It will call into the nodes
/// query their metrics, then send them to blockvisor-api.
async fn node_metrics(nodes: Arc<RwLock<Nodes>>, endpoint: &Endpoint, token: api::AuthToken) {
    let mut timer = tokio::time::interval(node_metrics::COLLECT_INTERVAL);
    let channel = wait_for_channel(endpoint).await;
    let mut client = api::MetricsClient::with_auth(channel, token);
    loop {
        timer.tick().await;
        let mut lock = nodes.write().await;
        let metrics = blockvisord::node_metrics::collect_metrics(lock.nodes.values_mut()).await;
        // Drop the lock as early as possible.
        drop(lock);
        let metrics: pb::NodeMetricsRequest = metrics.into();
        if let Err(e) = client.node(metrics).await {
            error!("Could not send node metrics! `{e}`");
        }
    }
}

async fn host_metrics(host_id: String, endpoint: &Endpoint, token: api::AuthToken) {
    let mut timer = tokio::time::interval(hosts::COLLECT_INTERVAL);
    let channel = wait_for_channel(endpoint).await;
    let mut client = api::MetricsClient::with_auth(channel, token);
    loop {
        timer.tick().await;
        match blockvisord::hosts::get_host_metrics() {
            Ok(metrics) => {
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
