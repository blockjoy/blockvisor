use anyhow::{bail, Result};
use blockvisord::{
    cli::{App, ChainCommand, Command, HostCommand, NodeCommand},
    client::{APIClient, HostCreateRequest},
    config::Config,
    containers::{ContainerState, Containers},
    dbus::NodeProxy,
    hosts::{get_host_info, get_ip_address},
    systemd::{ManagerProxy, UnitStartMode, UnitStopMode},
};
use clap::Parser;
use tokio::time::Duration;
use zbus::Connection;

#[tokio::main]
async fn main() -> Result<()> {
    let args = App::parse();
    println!("{:?}", args);
    let timeout = Duration::from_secs(10);

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

            let client = APIClient::new(&cmd_args.blockjoy_api_url, timeout)?;
            let host = client.register_host(&cmd_args.otp, &create).await?;

            Config {
                id: host.id.to_string(),
                token: host.token,
                blockjoy_api_url: cmd_args.blockjoy_api_url,
            }
            .save()
            .await?;

            if !Containers::exists() {
                Containers::default().save().await?;
            }
        }
        Command::Start(_) => {
            if !Config::exists() {
                bail!("Host is not registered, please run `init` first");
            }

            // Enable the service to start on host bootup and start it.
            println!("Enabling blockvisor service to start on host boot.");
            systemd_manager_proxy
                .enable_unit_files(&["blockvisor.service"], false, false)
                .await?;
            println!("Starting blockvisor service");
            systemd_manager_proxy
                .start_unit("blockvisor.service", UnitStartMode::Fail)
                .await?;

            println!("blockvisor service started successfully");
        }
        Command::Stop(_) => {
            println!("Stopping blockvisor service");
            systemd_manager_proxy
                .stop_unit("blockvisor.service", UnitStopMode::Fail)
                .await?;
            println!("blockvisor service stopped successfully");
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
        HostCommand::Network { command: _ } => todo!(),
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
            node_proxy
                .list()
                .await?
                .iter()
                .filter(|c| {
                    chain
                        .as_ref()
                        .map(|chain| c.chain.contains(chain))
                        .unwrap_or(true)
                        && (*all || c.state == ContainerState::Started)
                })
                .for_each(|c| println!("{:?}", c));
        }
        NodeCommand::Create { chain } => {
            let id = node_proxy.create(chain).await?;
            println!("Created new node for `{}` chain with ID `{}`", chain, id);
        }
        NodeCommand::Start { id } => {
            node_proxy.start(id).await?;
            println!("Started node with ID `{}`", id);
        }
        NodeCommand::Stop { id } => {
            node_proxy.stop(id).await?;
            println!("Stopped node with ID `{}`", id);
        }
        NodeCommand::Delete { id } => {
            node_proxy.delete(id).await?;
            println!("Deleted node with ID `{}`", id);
        }
        NodeCommand::Restart { id: _ } => todo!(),
        NodeCommand::Console { id: _ } => todo!(),
        NodeCommand::Logs { id: _ } => todo!(),
    }
    Ok(())
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
