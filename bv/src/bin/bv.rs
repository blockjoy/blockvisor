use anyhow::{bail, Result};
use blockvisord::config::SharedConfig;
use blockvisord::{
    cli::{App, ChainCommand, Command, HostCommand, NodeCommand},
    config::{Config, CONFIG_PATH},
    hosts::{HostInfo, HostMetrics},
    linux_platform::bv_root,
    pretty_table::{PrettyTable, PrettyTableRow},
    server::{
        bv_pb::blockvisor_client::BlockvisorClient,
        bv_pb::{self, Node, Parameter},
    },
    services::{
        api::{pb, HostsService},
        cookbook::CookbookService,
    },
};
use bv_utils::cmd::run_cmd;
use clap::{crate_version, Parser};
use cli_table::print_stdout;
use petname::Petnames;
use std::{collections::HashMap, io::BufRead};
use tokio::time::{sleep, Duration};
use tonic::transport;
use tonic::transport::Channel;
use uuid::Uuid;

// TODO: use proper wait mechanism
const BLOCKVISOR_START_TIMEOUT: Duration = Duration::from_secs(5);
const BLOCKVISOR_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    let args = App::parse();

    if !bv_root().join(CONFIG_PATH).exists() {
        bail!("Host is not registered, please run `bvup` first");
    }
    let config = Config::load(&bv_root()).await?;
    let bv_url = format!("http://localhost:{}", config.blockvisor_port);

    match args.command {
        Command::Start(_) => {
            if is_running(bv_url).await? {
                println!("Service already running");
                return Ok(());
            }

            run_cmd("systemctl", ["start", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_START_TIMEOUT).await;
            println!("blockvisor service started successfully");
        }
        Command::Stop(_) => {
            run_cmd("systemctl", ["stop", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_STOP_TIMEOUT).await;
            println!("blockvisor service stopped successfully");
        }
        Command::Status(_) => {
            if is_running(bv_url).await? {
                println!("Service running");
            } else {
                println!("Service stopped");
            }
        }
        Command::Host { command } => process_host_command(config, command).await?,
        Command::Chain { command } => process_chain_command(command).await?,
        Command::Node { command } => {
            NodeClient::new(format!("http://localhost:{}", config.blockvisor_port))
                .await?
                .process_node_command(command)
                .await?
        }
    }

    Ok(())
}

async fn is_running(url: String) -> Result<bool> {
    Ok(transport::Endpoint::from_shared(url)?
        .connect()
        .await
        .is_ok())
}

async fn process_host_command(config: Config, command: HostCommand) -> Result<()> {
    let to_gb = |n| n as f64 / 1_000_000_000.0;
    match command {
        HostCommand::Info => {
            let info = HostInfo::collect()?;
            println!("Hostname:       {:>10}", info.name);
            println!("OS name:        {:>10}", info.os);
            println!("OS version:     {:>10}", info.os_version);
            println!("CPU count:      {:>10}", info.cpu_count);
            println!("Total mem:      {:>10.3} GB", to_gb(info.mem_size));
            println!("Total disk:     {:>10.3} GB", to_gb(info.disk_size));
        }
        HostCommand::Update => {
            let info = HostInfo::collect()?;
            let mut client = HostsService::connect(config.clone()).await?;
            let update = pb::HostServiceUpdateRequest {
                id: config.id,
                name: Some(info.name),
                version: Some(crate_version!().to_string()),
                os: Some(info.os),
                os_version: Some(info.os_version),
            };
            client.update(update).await?;
        }
        HostCommand::Metrics => {
            let metrics = HostMetrics::collect()?;
            println!("Used cpu:       {:>10} %", metrics.used_cpu);
            println!("Used mem:       {:>10.3} GB", to_gb(metrics.used_memory));
            println!(
                "Used disk:      {:>10.3} GB",
                to_gb(metrics.used_disk_space)
            );
            println!("Load (1 min):   {:>10}", metrics.load_one);
            println!("Load (5 mins):  {:>10}", metrics.load_five);
            println!("Load (15 mins): {:>10}", metrics.load_fifteen);
            println!(
                "Network in:     {:>10.3} GB",
                to_gb(metrics.network_received)
            );
            println!("Network out:    {:>10.3} GB", to_gb(metrics.network_sent));
            println!("Uptime:         {:>10} seconds", metrics.uptime);
        }
    }

    Ok(())
}

#[allow(unreachable_code)]
async fn process_chain_command(command: ChainCommand) -> Result<()> {
    let config = SharedConfig::new(Config::load(&bv_root()).await?);

    match command {
        ChainCommand::List {
            protocol,
            r#type,
            number,
        } => {
            let mut cookbook_service = CookbookService::connect(bv_root(), &config).await?;
            let mut versions = cookbook_service.list_versions(&protocol, &r#type).await?;

            versions.truncate(number);

            for version in versions {
                println!("{version}");
            }
        }
    }

    Ok(())
}

struct NodeClient {
    client: BlockvisorClient<Channel>,
}

impl NodeClient {
    async fn new(url: String) -> Result<Self> {
        Ok(Self {
            client: BlockvisorClient::connect(url).await?,
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

    async fn list_capabilities(&mut self, node_id: Uuid) -> Result<String> {
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
            .reduce(|msg, cap| msg + "\n" + cap.as_str())
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
            println!("Started node `{id}`");
        }
        Ok(())
    }

    async fn stop_nodes(&mut self, ids: &Vec<String>) -> Result<()> {
        for id in ids {
            self.client
                .stop_node(bv_pb::StopNodeRequest { id: id.clone() })
                .await?;
            println!("Stopped node `{id}`");
        }
        Ok(())
    }

    async fn process_node_command(mut self, command: NodeCommand) -> Result<()> {
        match command {
            NodeCommand::List { running } => {
                let nodes = self.fetch_nodes().await?;
                let mut nodes = nodes
                    .iter()
                    .filter(|n| (!running || (n.status() == bv_pb::NodeStatus::Running)))
                    .peekable();
                if nodes.peek().is_some() {
                    let mut table = vec![];
                    for node in nodes.cloned() {
                        let status = node.status();
                        table.push(PrettyTableRow {
                            id: node.id,
                            name: node.name,
                            image: fmt_opt(node.image),
                            status,
                            ip: node.ip,
                            uptime: fmt_opt(node.uptime),
                        })
                    }
                    print_stdout(table.to_pretty_table())?;
                } else {
                    println!("No nodes found.");
                }
            }
            NodeCommand::Create {
                image,
                ip,
                gateway,
                props,
            } => {
                let id = Uuid::new_v4();
                let name = Petnames::default().generate_one(3, "_");
                let node_image = parse_image(&image)?;
                let props: HashMap<String, String> = props
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or_default();
                let properties = props
                    .into_iter()
                    .map(|(name, value)| Parameter { name, value })
                    .collect();
                self.client
                    .create_node(bv_pb::CreateNodeRequest {
                        id: id.to_string(),
                        name: name.clone(),
                        image: Some(node_image),
                        ip,
                        gateway,
                        properties,
                    })
                    .await?;
                println!("Created new node from `{image}` image with ID `{id}` and name `{name}`");
            }
            NodeCommand::Upgrade { id_or_names, image } => {
                let node_image = parse_image(&image)?;
                for id_or_name in id_or_names {
                    let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                    self.client
                        .upgrade_node(bv_pb::UpgradeNodeRequest {
                            id: id.clone(),
                            image: Some(node_image.clone()),
                        })
                        .await?;
                    println!("Upgraded node `{id}` to `{image}` image");
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
            NodeCommand::Delete {
                id_or_names,
                all,
                yes,
            } => {
                // We only respect the `--all` flag when `id_or_names` is empty, in order to
                // prevent a typo from accidentally deleting all nodes.
                let id_or_names = if id_or_names.is_empty() && all {
                    let confirm = ask_confirm("Are you sure you want to delete all nodes?", yes)?;
                    if !confirm {
                        return Ok(());
                    }

                    self.fetch_nodes()
                        .await?
                        .into_iter()
                        .map(|n| n.id)
                        .collect()
                } else {
                    id_or_names
                };
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
            NodeCommand::Logs { id_or_name } => {
                let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                let logs = self
                    .client
                    .get_node_logs(bv_pb::GetNodeLogsRequest { id: id.clone() })
                    .await?;
                for log in logs.into_inner().logs {
                    print!("{log}");
                }
            }
            NodeCommand::BabelLogs {
                id_or_name,
                max_lines,
            } => {
                let id = self.resolve_id_or_name(&id_or_name).await?.to_string();
                let logs = self
                    .client
                    .get_babel_logs(bv_pb::GetBabelLogsRequest {
                        id: id.clone(),
                        max_lines,
                    })
                    .await?;
                for log in logs.into_inner().logs {
                    print!("{log}");
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
                    println!("{name}");
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
                param,
            } => {
                let node_id = self.resolve_id_or_name(&id_or_name).await?;
                let req = bv_pb::BlockchainRequest {
                    method,
                    node_id: node_id.to_string(),
                    param: param.unwrap_or_default(),
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
                println!(
                    "App Status:     {:>10}",
                    fmt_opt(metrics.application_status)
                );
                println!("Sync Status:    {:>10}", fmt_opt(metrics.sync_status));
            }
        }
        Ok(())
    }
}

/// Requests confirmation from the user, i.e. the user must type `y` to continue.
///
/// ### Params
/// msg:    the message that is displayed to the user to request access. On display this function
///         will append ` [y/N]` to the message.
/// dash_y: if this flag is true, requesting user input is skippend and `true` is immediately
///         returned.
fn ask_confirm(msg: &str, dash_y: bool) -> Result<bool> {
    if dash_y {
        return Ok(true);
    }
    println!("{msg} [y/N]:");
    let mut input = String::new();
    std::io::stdin().lock().read_line(&mut input)?;
    Ok(input.trim().to_lowercase() == "y")
}

fn fmt_opt<T: std::fmt::Display>(opt: Option<T>) -> String {
    opt.map(|t| t.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn parse_image(image: &str) -> Result<bv_pb::NodeImage> {
    let image_vec: Vec<&str> = image.split('/').collect();
    if image_vec.len() != 3 {
        bail!("Wrong number of components in image: {image:?}");
    }
    Ok(bv_pb::NodeImage {
        protocol: image_vec[0].to_string(),
        node_type: image_vec[1].to_string(),
        node_version: image_vec[2].to_string(),
    })
}
