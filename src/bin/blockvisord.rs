use anyhow::Result;
use blockvisord::{
    client::{APIClient, CommandStatusUpdate},
    config::Config,
    dbus::NodeProxy,
    grpc::{self, pb::node_command::Command},
    logging::setup_logging,
    nodes::Nodes,
};
use std::str::FromStr;
use tokio::{
    sync::broadcast::Receiver,
    time::{sleep, Duration},
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::transport::{Channel, Endpoint};
use tracing::{error, info};
use uuid::Uuid;
use zbus::{Connection, ConnectionBuilder, ProxyDefault};

#[allow(unreachable_code)]
#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    info!("Starting...");

    let config = Config::load().await?;
    let nodes = Nodes::load().await?;
    let updates_tx = nodes.get_updates_sender().await?.clone();

    let _conn = ConnectionBuilder::system()?
        .name(NodeProxy::DESTINATION)?
        .serve_at(NodeProxy::PATH, nodes)?
        .build()
        .await?;

    let token = grpc::AuthToken(config.token.to_owned());
    let endpoint = Endpoint::from_str(&config.blockjoy_api_url)?;
    let channel = wait_for_channel(&endpoint).await;

    info!("Creating gRPC client...");
    let mut client = grpc::Client::with_auth(channel, token);

    loop {
        let updates_rx = updates_tx.subscribe();
        if let Err(e) = process_commands_stream(&mut client, updates_rx).await {
            error!("Error processing pending commands: {:?}", e);
        }
        sleep(Duration::from_secs(5)).await;
    }

    info!("Stopping...");
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

async fn process_commands_stream(
    client: &mut grpc::Client,
    rx: Receiver<grpc::pb::InfoUpdate>,
) -> Result<()> {
    info!("Processing pending commands");
    let updates_stream = BroadcastStream::new(rx).filter_map(|item| item.ok());

    let response = client.commands(updates_stream).await?;
    let mut commands_stream = response.into_inner();

    let conn = Connection::system().await?;
    let node_proxy = NodeProxy::new(&conn).await?;

    info!("Getting pending commands from stream...");
    while let Some(received) = commands_stream.next().await {
        info!("received: {received:?}");
        let received = received?;
        match received.r#type {
            Some(grpc::pb::command::Type::Node(node)) => {
                let node_id = node.id.unwrap().value;
                let node_id = Uuid::from_str(&node_id)?;
                match node.command {
                    Some(cmd) => match cmd {
                        Command::Create(args) => {
                            node_proxy
                                .create(&node_id, &args.name, &args.image.unwrap().url)
                                .await?;
                        }
                        Command::Delete(_) => unimplemented!(),
                        Command::Start(_) => unimplemented!(),
                        Command::Stop(_) => unimplemented!(),
                        Command::Restart(_) => unimplemented!(),
                        Command::Upgrade(_) => unimplemented!(),
                        Command::Update(_) => unimplemented!(),
                        Command::InfoGet(_) => unimplemented!(),
                        Command::Generic(_) => unimplemented!(),
                    },
                    None => unimplemented!(),
                };
            }
            Some(grpc::pb::command::Type::Host(_host)) => unimplemented!(),
            None => unimplemented!(),
        };
    }

    Ok(())
}

#[allow(dead_code)]
async fn process_pending_commands(config: &Config) -> Result<()> {
    let timeout = Duration::from_secs(10);
    let client = APIClient::new(&config.blockjoy_api_url, timeout)?;

    info!("Getting pending commands for host: {}", &config.id);
    for command in client
        .get_pending_commands(&config.token, &config.id)
        .await?
    {
        info!("Processing command: {}", &command.cmd);

        let update = CommandStatusUpdate {
            response: "Done".to_string(),
            exit_status: 0,
        };

        client
            .update_command_status(&config.token, &command.id, &update)
            .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use blockvisord::client::{APIClient, CommandStatusUpdate};
    use chrono::{TimeZone, Utc};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn test_get_pending_commands() {
        let server = MockServer::start();

        let token = "TOKEN";
        let host_id = "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b";

        let m = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/hosts/{}/commands/pending", host_id))
                .header("Content-Type", "application/json")
                .header("authorization", format!("Bearer {}", token));
            then.status(200)
                .header("Content-Type", "application/json")
                .json_body(json!([
                  {
                    "id": "497f6eca-6276-4993-bfeb-53cbbbba6f08",
                    "host_id": host_id,
                    "cmd": "restart_miner",
                    "created_at": "2019-08-24T14:15:22Z",
                  }
                ]));
        });

        let client = APIClient::new(&server.base_url(), Duration::from_secs(10)).unwrap();
        let resp = client.get_pending_commands(token, host_id).await.unwrap();

        assert_eq!(resp.len(), 1);
        assert_eq!(resp[0].id, "497f6eca-6276-4993-bfeb-53cbbbba6f08");
        assert_eq!(resp[0].host_id, host_id);
        assert_eq!(resp[0].cmd, "restart_miner");
        assert_eq!(resp[0].sub_cmd, None);
        assert_eq!(resp[0].response, None);
        assert_eq!(resp[0].exit_status, None);
        assert_eq!(resp[0].created_at, Utc.ymd(2019, 8, 24).and_hms(14, 15, 22));
        assert_eq!(resp[0].completed_at, None);

        m.assert();
    }

    #[tokio::test]
    async fn test_update_command_status() {
        let server = MockServer::start();

        let token = "TOKEN";
        let command_id = "497f6eca-6276-4993-bfeb-53cbbbba6f08";

        let m = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/commands/{}/response", command_id))
                .header("Content-Type", "application/json")
                .header("authorization", format!("Bearer {}", token))
                .json_body(json!({
                    "response": "restarted",
                    "exit_status": 0_i32,
                }));
            then.status(200)
                .header("Content-Type", "application/json")
                .json_body(json!(
                  {
                    "id": command_id,
                    "host_id": "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b",
                    "cmd": "restart_miner",
                    "response": "restarted",
                    "exit_status": 0_i32,
                    "created_at": "2019-08-24T14:15:22Z",
                    "completed_at": "2020-08-24T14:15:22Z",
                  }
                ));
        });

        let client = APIClient::new(&server.base_url(), Duration::from_secs(10)).unwrap();
        let update = CommandStatusUpdate {
            response: "restarted".to_string(),
            exit_status: 0,
        };
        let resp = client
            .update_command_status(token, command_id, &update)
            .await
            .unwrap();

        assert_eq!(resp.id, command_id);
        assert_eq!(resp.host_id, "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b");
        assert_eq!(resp.cmd, "restart_miner");
        assert_eq!(resp.sub_cmd, None);
        assert_eq!(resp.response, Some("restarted".to_string()));
        assert_eq!(resp.exit_status, Some(0_i32));
        assert_eq!(resp.created_at, Utc.ymd(2019, 8, 24).and_hms(14, 15, 22));
        assert_eq!(
            resp.completed_at,
            Some(Utc.ymd(2020, 8, 24).and_hms(14, 15, 22))
        );

        m.assert();
    }
}
