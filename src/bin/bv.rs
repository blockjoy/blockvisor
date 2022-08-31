use anyhow::{bail, Result};
use blockvisord::{
    cli::{App, ChainCommand, Command, HostCommand, NodeCommand},
    client::{APIClient, HostCreateRequest},
    config::Config,
    dbus::NodeProxy,
    hosts::{get_host_info, get_ip_address},
    node_data::NodeStatus,
    nodes::Nodes,
    pretty_table::PrettyTable,
    systemd::{ManagerProxy, UnitStartMode, UnitStopMode},
};
use clap::Parser;
use cli_table::print_stdout;
use petname::Petnames;
use std::future::ready;
use tokio::time::{timeout, Duration};
use uuid::Uuid;
use zbus::{export::futures_util::StreamExt, fdo, names::BusName, Connection, ProxyDefault};

const API_TIMEOUT: Duration = Duration::from_secs(10);
const BLOCKVISOR_START_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    let args = App::parse();

    let conn = Connection::system().await?;
    let systemd_manager_proxy = ManagerProxy::new(&conn).await?;

    match args.command {
        Command::Init(cmd_args) => {
            println!("Configuring blockvisor");

            let ip = get_ip_address(&cmd_args.ifa);
            let info = get_host_info();

            let create = HostCreateRequest {
                org_id: None,
                name: info.name.unwrap(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
                location: None,
                cpu_count: info.cpu_count,
                mem_size: info.mem_size,
                disk_size: info.disk_size,
                os: info.os,
                os_version: info.os_version,
                ip_addr: ip,
                val_ip_addrs: None,
            };
            println!("{:?}", create);

            let client = APIClient::new(&cmd_args.blockjoy_api_url, API_TIMEOUT)?;
            let host = client.register_host(&cmd_args.otp, &create).await?;

            Config {
                id: host.id.to_string(),
                token: host.token,
                blockjoy_api_url: cmd_args.blockjoy_api_url,
            }
            .save()
            .await?;

            if !Nodes::exists() {
                Nodes::default().save().await?;
            }
        }
        Command::Reset(cmd_args) => {
            let confirm = if cmd_args.yes {
                true
            } else {
                let mut input = String::new();
                println!(
                    "Are you sure you want to delete all nodes and remove the host from API? [y/N]:"
                );
                std::io::stdin().read_line(&mut input)?;
                input.trim().to_lowercase() == "y"
            };

            if confirm {
                let node_proxy = NodeProxy::new(&conn).await?;
                let nodes = node_proxy.list().await?;
                for node in nodes {
                    let id = node.id;
                    println!("Deleting node with ID `{}`", &id);
                    node_proxy.delete(&id).await?;
                }

                let config = Config::load().await?;
                let url = config.blockjoy_api_url;
                let host_id = config.id;
                let client = APIClient::new(&url, API_TIMEOUT)?;
                println!("Deleting host `{host_id}` from API `{url}`");
                client.delete_host(&config.token, &host_id).await?;

                Config::remove().await?;
            }
        }
        Command::Start(_) => {
            if !Config::exists() {
                bail!("Host is not registered, please run `init` first");
            }

            let dbus_proxy = fdo::DBusProxy::new(&conn).await.unwrap();
            if dbus_proxy
                .name_has_owner(BusName::from_static_str(NodeProxy::DESTINATION)?)
                .await?
            {
                println!("Service already running");
                return Ok(());
            }

            // Enable the blockvisor service and babel socket to start on host bootup and start it.
            println!("Enabling blockvisor service to start on host boot.");
            systemd_manager_proxy
                .enable_unit_files(&["blockvisor.service", "babel-bus.socket"], false, false)
                .await?;

            println!("Starting babel socket unit");
            systemd_manager_proxy
                .start_unit("babel-bus.socket", UnitStartMode::Fail)
                .await?;
            println!("babel socket setup");

            println!("Starting blockvisor service");
            let node_proxy = NodeProxy::new(&conn).await?;
            let mut stream = node_proxy
                .receive_owner_changed()
                .await?
                .filter(|unique_name| {
                    // Look for the first name-owned event.
                    ready(unique_name.is_some())
                });
            systemd_manager_proxy
                .start_unit("blockvisor.service", UnitStartMode::Fail)
                .await?;
            timeout(BLOCKVISOR_START_TIMEOUT, stream.next()).await?;

            println!("blockvisor service started successfully");
        }
        Command::Stop(_) => {
            println!("Stopping blockvisor service");
            systemd_manager_proxy
                .stop_unit("blockvisor.service", UnitStopMode::Fail)
                .await?;
            println!("blockvisor service stopped successfully");

            println!("Shutting down babel socket unit");
            systemd_manager_proxy
                .stop_unit("babel-bus.socket", UnitStopMode::Fail)
                .await?;
            println!("babel socket terminated");
        }
        Command::Status(_) => {
            todo!()
        }
        Command::Host { command } => process_host_command(&command).await?,
        Command::Chain { command } => process_chain_command(&command).await?,
        Command::Node { command } => process_node_command(&command).await?,
    }

    Ok(())
}

async fn process_host_command(command: &HostCommand) -> Result<()> {
    match command {
        HostCommand::Info => {
            let info = get_host_info();
            println!("{:?}", info);
        }
    }

    Ok(())
}

#[allow(unreachable_code)]
async fn process_chain_command(command: &ChainCommand) -> Result<()> {
    match command {
        ChainCommand::List => todo!(),
        ChainCommand::Status { id: _ } => todo!(),
        ChainCommand::Sync { id: _ } => todo!(),
    }

    Ok(())
}

async fn process_node_command(command: &NodeCommand) -> Result<()> {
    let conn = Connection::system().await?;
    let node_proxy = NodeProxy::new(&conn).await?;

    match command {
        NodeCommand::List { all, chain } => {
            let nodes = node_proxy.list().await?;
            let mut nodes = nodes
                .iter()
                .filter(|c| {
                    chain
                        .as_ref()
                        .map(|chain| c.chain.contains(chain))
                        .unwrap_or(true)
                        && (*all || c.status == NodeStatus::Running)
                })
                .peekable();
            if nodes.peek().is_some() {
                print_stdout(nodes.to_pretty_table())?;
            } else {
                println!("No nodes found.");
            }
        }
        NodeCommand::Create { chain } => {
            let id = Uuid::new_v4();
            let name = Petnames::default().generate_one(3, "-");
            node_proxy.create(&id, &name, chain).await?;
            println!(
                "Created new node for `{}` chain with ID `{}` and name `{}`",
                chain, id, name
            );
        }
        NodeCommand::Start { id_or_name } => {
            let id = resolve_id_or_name(&node_proxy, id_or_name).await?;
            node_proxy.start(&id).await?;
            println!("Started node `{}`", id_or_name);
        }
        NodeCommand::Stop { id_or_name } => {
            let id = resolve_id_or_name(&node_proxy, id_or_name).await?;
            node_proxy.stop(&id).await?;
            println!("Stopped node `{}`", id_or_name);
        }
        NodeCommand::Delete { id_or_name } => {
            let id = resolve_id_or_name(&node_proxy, id_or_name).await?;
            node_proxy.delete(&id).await?;
            println!("Deleted node `{}`", id_or_name);
        }
        NodeCommand::Restart { id_or_name: _ } => todo!(),
        NodeCommand::Console { id_or_name: _ } => todo!(),
        NodeCommand::Logs { id_or_name: _ } => todo!(),
        NodeCommand::Status { id_or_name } => {
            let id = resolve_id_or_name(&node_proxy, id_or_name).await?;
            let status = node_proxy.status(&id).await?;
            println!("{}", status);
        }
    }
    Ok(())
}

async fn resolve_id_or_name(node_proxy: &NodeProxy<'_>, id_or_name: &str) -> Result<Uuid> {
    let uuid = match Uuid::parse_str(id_or_name) {
        Ok(v) => v,
        Err(_) => node_proxy.node_id_for_name(id_or_name).await?,
    };

    Ok(uuid)
}

#[cfg(test)]
mod tests {
    use blockvisord::client::{APIClient, HostCreateRequest};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::time::Duration;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_register_host() {
        let server = MockServer::start();
        let org_id = Uuid::new_v4();

        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/host_provisions/OTP/hosts")
                .header("Content-Type", "application/json")
                .json_body(json!({
                    "org_id": org_id,
                    "name": "some-host",
                    "version": "1.0",
                    "location": null,
                    "cpu_count": 4_i64,
                    "mem_size": 8_i64,
                    "disk_size": 100_i64,
                    "os": "ubuntu",
                    "os_version": "4.14.12",
                    "ip_addr": "192.168.0.1",
                    "val_ip_addrs": null,
                }));
            then.status(200)
                .header("Content-Type", "application/json")
                .json_body(json!({
                    "id": "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b",
                    "token": "secret_token",
                    "org_id": org_id,
                    "name": "some-host",
                    "version": "1.0",
                    "location": null,
                    "cpu_count": 4_i64,
                    "mem_size": 8_i64,
                    "disk_size": 100_i64,
                    "os": "ubuntu",
                    "os_version": "4.14.12",
                    "ip_addr": "192.168.0.1",
                    "val_ip_addrs": null,
                    "created_at": "2019-08-24T14:15:22Z",
                    "status": "online",
                    "validators": [],
                }));
        });

        let client = APIClient::new(&server.base_url(), Duration::from_secs(10)).unwrap();
        let otp = "OTP";
        let info = HostCreateRequest {
            org_id: Some(org_id),
            name: "some-host".to_string(),
            version: Some("1.0".to_string()),
            location: None,
            cpu_count: Some(4),
            mem_size: Some(8),
            disk_size: Some(100),
            os: Some("ubuntu".to_string()),
            os_version: Some("4.14.12".to_string()),
            ip_addr: "192.168.0.1".to_string(),
            val_ip_addrs: None,
        };
        let resp = client.register_host(otp, &info).await.unwrap();

        assert_eq!(resp.id.to_string(), "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b");
        assert_eq!(resp.token, "secret_token");

        m.assert();
    }
}
