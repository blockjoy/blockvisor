use anyhow::Result;
use blockvisord::{
    config::Config,
    grpc,
    logging::setup_logging,
    node_data::NodeStatus,
    nodes::Nodes,
    server::{bv_pb, BlockvisorServer, BLOCKVISOR_SERVICE_PORT},
};
use std::{net::ToSocketAddrs, str::FromStr, sync::Arc};
use tokio::{
    sync::Mutex,
    time::{sleep, Duration},
};
use tonic::transport::{Channel, Endpoint, Server};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    info!("Starting...");

    let config = Config::load().await?;
    let nodes = Nodes::load(config.clone()).await?;
    let updates_tx = nodes.get_updates_sender().await?.clone();
    let nodes = Arc::new(Mutex::new(nodes));

    let url = format!("0.0.0.0:{BLOCKVISOR_SERVICE_PORT}");
    let server = BlockvisorServer {
        nodes: nodes.clone(),
    };
    let internal_api_server_future = create_server(url, server);

    let token = grpc::AuthToken(config.token.to_owned());
    let endpoint = Endpoint::from_str(&config.blockjoy_api_url)?;
    let external_api_client_future = async {
        let channel = wait_for_channel(&endpoint).await;

        info!("Creating gRPC client...");
        let mut client = grpc::Client::with_auth(channel, token);

        loop {
            if let Err(e) =
                grpc::process_commands_stream(&mut client, nodes.clone(), updates_tx.clone()).await
            {
                error!("Error processing pending commands: {:?}", e);
            }
            sleep(Duration::from_secs(5)).await;
        }
    };

    let nodes_recovery_future = async {
        loop {
            let list = nodes.lock().await.list().await;

            for node in list {
                let id = &node.id;
                if node.status() == NodeStatus::Failed {
                    match node.expected_status {
                        NodeStatus::Running => {
                            info!("Recovery: starting node with ID `{id}`");
                            if let Err(e) = nodes.lock().await.start(node.id).await {
                                error!("Recovery: starting node with ID `{id}` failed: {e}");
                            }
                        }
                        NodeStatus::Stopped => {
                            info!("Recovery: stopping node with ID `{id}`");
                            if let Err(e) = nodes.lock().await.stop(node.id).await {
                                error!("Recovery: stopping node with ID `{id}` failed: {e}",);
                            }
                        }
                        NodeStatus::Failed => {
                            info!("Recovery: node with ID `{id}` cannot be recovered");
                        }
                    }
                }
            }
            sleep(Duration::from_secs(5)).await;
        }
    };

    tokio::select! {
        _ = internal_api_server_future => {},
        _ = external_api_client_future => {},
        _ = nodes_recovery_future => {},
    }

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
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
