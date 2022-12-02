use anyhow::{bail, Result};
use blockvisord::{
    cli::{App, ChainCommand, Command, HostCommand, NodeCommand},
    config::Config,
    grpc::pb,
    hosts::{get_host_info, get_host_metrics, get_ip_address},
    nodes::{CommonData, Nodes},
    pretty_table::{PrettyTable, PrettyTableRow},
    server::{
        bv_pb, bv_pb::blockvisor_client::BlockvisorClient, bv_pb::Node, BlockvisorServer,
        BLOCKVISOR_SERVICE_URL,
    },
    utils::run_cmd,
};
use clap::{crate_version, Parser};
use cli_table::print_stdout;
use petname::Petnames;
use tokio::time::{sleep, Duration};
use tonic::transport::Channel;
use uuid::Uuid;

// TODO: use proper wait mechanism
const BLOCKVISOR_START_TIMEOUT: Duration = Duration::from_secs(5);
const BLOCKVISOR_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    let args = App::parse();

    match args.command {
        Command::Init(cmd_args) => {
            println!("Configuring blockvisor");

            let ip = get_ip_address(&cmd_args.ifa);
            let host_info = get_host_info();

            let info = pb::HostInfo {
                id: None,
                name: host_info.name,
                version: Some(crate_version!().to_string()),
                location: None,
                cpu_count: host_info.cpu_count,
                mem_size: host_info.mem_size,
                disk_size: host_info.disk_size,
                os: host_info.os,
                os_version: host_info.os_version,
                ip: Some(ip),
                ip_range_to: None,
                ip_range_from: None,
                ip_gateway: None,
            };
            let create = pb::ProvisionHostRequest {
                request_id: Some(Uuid::new_v4().to_string()),
                otp: cmd_args.otp,
                info: Some(info),
                status: pb::ConnectionStatus::Online.into(),
            };
            println!("{:?}", create);

            let mut client =
                pb::hosts_client::HostsClient::connect(cmd_args.blockjoy_api_url.clone()).await?;

            let host = client.provision(create).await?.into_inner();

            let api_config = Config {
                id: host.host_id,
                token: host.token,
                blockjoy_api_url: cmd_args.blockjoy_api_url,
            };
            api_config.save().await?;

            if !Nodes::exists() {
                let nodes_data = CommonData { machine_index: 0 };
                Nodes::new(api_config, nodes_data).save().await?;
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

            let mut service_client = BlockvisorClient::connect(BLOCKVISOR_SERVICE_URL).await?;

            if confirm {
                let nodes = service_client
                    .get_nodes(bv_pb::GetNodesRequest {})
                    .await?
                    .into_inner()
                    .nodes;
                for node in nodes {
                    let id = node.id;
                    println!("Deleting node with ID `{}`", &id);
                    service_client
                        .delete_node(bv_pb::DeleteNodeRequest { id })
                        .await?;
                }

                let config = Config::load().await?;
                let url = config.blockjoy_api_url;
                let host_id = config.id;

                let delete = pb::DeleteHostRequest {
                    request_id: Some(Uuid::new_v4().to_string()),
                    host_id: host_id.clone(),
                };
                let mut client = pb::hosts_client::HostsClient::connect(url.clone()).await?;
                println!("Deleting host `{host_id}` from API `{url}`");
                client.delete(delete).await?;

                Config::remove().await?;
            }
        }
        Command::Start(_) => {
            if !Config::exists() {
                bail!("Host is not registered, please run `init` first");
            }

            if BlockvisorServer::is_running().await {
                println!("Service already running");
                return Ok(());
            }

            run_cmd("systemctl", &["start", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_START_TIMEOUT).await;
            println!("blockvisor service started successfully");
        }
        Command::Stop(_) => {
            run_cmd("systemctl", &["stop", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_STOP_TIMEOUT).await;
            println!("blockvisor service stopped successfully");
        }
        Command::Status(_) => {
            if !Config::exists() {
                bail!("Host is not registered, please run `init` first");
            }

            if BlockvisorServer::is_running().await {
                println!("Service running");
            } else {
                println!("Service stopped");
            }
        }
        Command::Host { command } => process_host_command(command).await?,
        Command::Chain { command } => process_chain_command(command).await?,
        Command::Node { command } => {
            NodeClient::new()
                .await?
                .process_node_command(command)
                .await?
        }
    }

    Ok(())
}

async fn process_host_command(command: HostCommand) -> Result<()> {
    match command {
        HostCommand::Info => {
            let info = get_host_info();
            println!("{:?}", info);
        }
        HostCommand::Metrics => {
            let metrics = get_host_metrics();
            println!("Used cpu:       {:>13} of 100", metrics.used_cpu);
            println!("Used mem:       {:>13} bytes", metrics.used_memory);
            println!("Used disk:      {:>13} bytes", metrics.used_disk_space);
            println!("Load (1 min):   {:>13} %", metrics.load_one);
            println!("Load (5 mins):  {:>13} %", metrics.load_five);
            println!("Load (15 mins): {:>13} %", metrics.load_fifteen);
            println!("Network in:     {:>13} bytes", metrics.network_received);
            println!("Network out:    {:>13} bytes", metrics.network_sent);
            println!("Uptime:         {:>13} seconds", metrics.uptime);
        }
    }

    Ok(())
}

#[allow(unreachable_code)]
async fn process_chain_command(command: ChainCommand) -> Result<()> {
    match command {
        ChainCommand::List => todo!(),
        ChainCommand::Status { id: _ } => todo!(),
        ChainCommand::Sync { id: _ } => todo!(),
    }

    Ok(())
}

struct NodeClient {
    client: BlockvisorClient<Channel>,
}

impl NodeClient {
    async fn new() -> Result<Self> {
        Ok(Self {
            client: BlockvisorClient::connect(BLOCKVISOR_SERVICE_URL).await?,
        })
    }

    async fn fetch_nodes(&mut self) -> Result<Vec<Node>> {
        Ok(self
            .client
            .get_nodes(bv_pb::GetNodesRequest {})
            .await?
            .into_inner()
            .nodes)
    }

    async fn list_capabilities(&mut self, node_id: uuid::Uuid) -> Result<String> {
        let req = bv_pb::ListCapabilitiesRequest {
            node_id: node_id.to_string(),
        };
        let caps = self
            .client
            .list_capabilities(req)
            .await?
            .into_inner()
            .capabilities
            .into_iter()
            .reduce(|msg, cap| msg + "\n" + &cap)
            .unwrap_or_default();
        Ok(caps)
    }

    async fn resolve_id_or_name(&mut self, id_or_name: &str) -> Result<Uuid> {
        let uuid = match Uuid::parse_str(id_or_name) {
            Ok(v) => v,
            Err(_) => {
                let uuid = self
                    .client
                    .get_node_id_for_name(bv_pb::GetNodeIdForNameRequest {
                        name: id_or_name.to_string(),
                    })
                    .await?
                    .into_inner()
                    .id;
                Uuid::parse_str(&uuid)?
            }
        };

        Ok(uuid)
    }

    async fn get_node_ids(&mut self, id_or_names: Vec<String>) -> Result<Vec<String>> {
        let mut ids: Vec<String> = Default::default();
        if id_or_names.is_empty() {
            for node in self.fetch_nodes().await? {
                ids.push(node.id);
            }
        } else {
            for id_or_name in id_or_names {
                ids.push(self.resolve_id_or_name(&id_or_name).await?.to_string());
            }
        };
        Ok(ids)
    }

    async fn start_nodes(&mut self, ids: &Vec<String>) -> Result<()> {
        for id in ids {
            self.client
                .start_node(bv_pb::StartNodeRequest { id: id.clone() })
                .await?;
            println!("Started node `{}`", id);
        }
        Ok(())
    }

    async fn stop_nodes(&mut self, ids: &Vec<String>) -> Result<()> {
        for id in ids {
            self.client
                .stop_node(bv_pb::StopNodeRequest { id: id.clone() })
                .await?;
            println!("Stopped node `{}`", id);
        }
        Ok(())
    }

    async fn process_node_command(mut self, command: NodeCommand) -> Result<()> {
        match command {
            NodeCommand::List { running, image } => {
                let nodes = self.fetch_nodes().await?;
                let mut nodes = nodes
                    .iter()
                    .filter(|n| {
                        image
                            .as_ref()
                            .map(|image| n.image.contains(image))
                            .unwrap_or(true)
                            && (!running || (n.status == bv_pb::NodeStatus::Running as i32))
                    })
                    .peekable();
                if nodes.peek().is_some() {
                    let mut table = vec![];
                    for node in nodes.cloned() {
                        table.push(PrettyTableRow {
                            id: node.id,
                            name: node.name,
                            image: node.image,
                            status: bv_pb::NodeStatus::from_i32(node.status).unwrap(),
                            ip: node.ip,
                        })
                    }
                    print_stdout(table.to_pretty_table())?;
                } else {
                    println!("No nodes found.");
                }
            }
            NodeCommand::Create { image, ip, gateway } => {
                let id = Uuid::new_v4();
                let name = Petnames::default().generate_one(3, "_");
                // TODO: this configurations is useful for testing on CI machine
                let gateway = gateway.unwrap_or_else(|| "216.18.214.193".to_string());
                let ip = ip.unwrap_or_else(|| "216.18.214.195".to_string());
                self.client
                    .create_node(bv_pb::CreateNodeRequest {
                        id: id.to_string(),
                        name: name.clone(),
                        image: image.to_string(),
                        ip,
                        gateway,
                    })
                    .await?;
                println!(
                    "Created new node from `{}` image with ID `{}` and name `{}`",
                    image, id, &name
                );
            }
            NodeCommand::Upgrade { id_or_names, image } => {
                for id_or_name in id_or_names {
                    let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                    self.client
                        .upgrade_node(bv_pb::UpgradeNodeRequest {
                            id: id.clone(),
                            image: image.to_string(),
                        })
                        .await?;
                    println!("Upgraded node `{}` to `{}` image", id, image);
                }
            }
            NodeCommand::Start { id_or_names } => {
                let ids = self.get_node_ids(id_or_names).await?;
                self.start_nodes(&ids).await?;
            }
            NodeCommand::Stop { id_or_names } => {
                let ids = self.get_node_ids(id_or_names).await?;
                self.stop_nodes(&ids).await?;
            }
            NodeCommand::Delete { id_or_names } => {
                for id_or_name in id_or_names {
                    let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                    self.client
                        .delete_node(bv_pb::DeleteNodeRequest { id })
                        .await?;
                    println!("Deleted node `{id_or_name}`");
                }
            }
            NodeCommand::Restart { id_or_names } => {
                let ids = self.get_node_ids(id_or_names).await?;
                self.stop_nodes(&ids).await?;
                self.start_nodes(&ids).await?;
            }
            NodeCommand::Console { id_or_name: _ } => todo!(),
            NodeCommand::Logs { id_or_name } => {
                let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                let logs = self
                    .client
                    .get_node_logs(bv_pb::GetNodeLogsRequest { id: id.clone() })
                    .await?;
                for log in logs.into_inner().logs {
                    print!("{}", log);
                }
            }
            NodeCommand::Status { id_or_names } => {
                for id_or_name in id_or_names {
                    let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                    let status = self
                        .client
                        .get_node_status(bv_pb::GetNodeStatusRequest { id })
                        .await?;
                    let status = status.into_inner().status;
                    match bv_pb::NodeStatus::from_i32(status) {
                        Some(status) => println!("{status}"),
                        None => eprintln!("Invalid status {status}"),
                    }
                }
            }
            NodeCommand::Keys { id_or_name } => {
                let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                let keys = self
                    .client
                    .get_node_keys(bv_pb::GetNodeKeysRequest { id })
                    .await?;
                for name in keys.into_inner().names {
                    println!("{}", name);
                }
            }
            NodeCommand::Capabilities { id_or_name } => {
                let node_id = self.resolve_id_or_name(&id_or_name).await?;
                let caps = self.list_capabilities(node_id).await?;
                print!("{caps}");
            }
            NodeCommand::Run {
                id_or_name,
                method,
                payload,
            } => {
                let node_id = self.resolve_id_or_name(&id_or_name).await?;
                let req = bv_pb::BlockchainRequest {
                    method,
                    node_id: node_id.to_string(),
                    payload,
                };
                match self.client.blockchain(req).await {
                    Ok(result) => println!("{}", result.into_inner().value),
                    Err(e) => {
                        if e.message().contains("not found") {
                            let msg = "Method not found. Options are:";
                            let caps = self.list_capabilities(node_id).await?;
                            bail!("{msg}\n{caps}");
                        }
                        return Err(anyhow::Error::from(e));
                    }
                }
            }
            NodeCommand::Metrics { id_or_name } => {
                let node_id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                let metrics = self
                    .client
                    .get_node_metrics(bv_pb::GetNodeMetricsRequest { node_id })
                    .await?
                    .into_inner();
                println!("Block height:   {:>10}", fmt_opt(metrics.height));
                println!("Block age:      {:>10}", fmt_opt(metrics.block_age));
                println!("Staking Status: {:>10}", fmt_opt(metrics.staking_status));
                println!("In consensus:   {:>10}", fmt_opt(metrics.consensus));
            }
        }
        Ok(())
    }
}

fn fmt_opt<T: std::fmt::Display>(opt: Option<T>) -> String {
    opt.map(|t| t.to_string())
        .unwrap_or_else(|| "-".to_string())
}
