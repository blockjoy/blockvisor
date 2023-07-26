use anyhow::{bail, Result};
use babel_api::engine::JobStatus;
use blockvisord::{
    cli::{App, ChainCommand, Command, HostCommand, NodeCommand, WorkspaceCommand},
    config::{Config, SharedConfig, CONFIG_PATH},
    hosts::{self, HostInfo, HostMetrics},
    internal_server,
    linux_platform::bv_root,
    node_data::{NodeDisplayInfo, NodeImage, NodeStatus},
    nodes::NodeConfig,
    pretty_table::{PrettyTable, PrettyTableRow},
    services::cookbook::CookbookService,
    workspace,
};
use bv_utils::cmd::{ask_confirm, run_cmd};
use clap::Parser;
use cli_table::print_stdout;
use petname::Petnames;
use std::{collections::HashMap, fs};
use tokio::time::{sleep, Duration};
use tonic::{transport, transport::Channel, Code};
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
    let bv_root = bv_root();
    let config = SharedConfig::new(Config::load(&bv_root).await?, bv_root);
    let port = config.read().await.blockvisor_port;
    let bv_url = format!("http://localhost:{port}");

    match args.command {
        Command::Start(_) => {
            if is_running(bv_url.clone()).await? {
                println!("Service already running");
                return Ok(());
            }

            run_cmd("systemctl", ["start", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_START_TIMEOUT).await;

            match is_running(bv_url.clone()).await {
                Ok(true) => println!("blockvisor service started successfully"),
                Ok(false) => {
                    bail!("blockvisor service did not start: cannot connect to `{bv_url}`");
                }
                Err(e) => bail!("blockvisor service did not start: {e:?}"),
            }
        }
        Command::Stop(_) => {
            run_cmd("systemctl", ["stop", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_STOP_TIMEOUT).await;

            match is_running(bv_url).await {
                Ok(true) => bail!("blockvisor service did not stop"),
                _ => println!("blockvisor service stopped successfully"),
            }
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
        Command::Node { command } => match NodeClient::new(bv_url).await {
            Ok(client) => client.process_node_command(command).await?,
            Err(e) => bail!("service is not running: {e:?}"),
        },
        Command::Workspace { command } => process_workspace_command(command).await?,
    }

    Ok(())
}

async fn is_running(url: String) -> Result<bool> {
    Ok(transport::Endpoint::from_shared(url)?
        .connect()
        .await
        .is_ok())
}

async fn process_host_command(config: SharedConfig, command: HostCommand) -> Result<()> {
    let to_gb = |n| n as f64 / 1_000_000_000.0;
    match command {
        HostCommand::Info => {
            let info = HostInfo::collect()?;
            println!("Hostname:       {:>10}", info.name);
            println!("OS name:        {:>10}", info.os);
            println!("OS version:     {:>10}", info.os_version);
            println!("CPU count:      {:>10}", info.cpu_count);
            println!("Total mem:      {:>10.3} GB", to_gb(info.memory_bytes));
            println!("Total disk:     {:>10.3} GB", to_gb(info.disk_space_bytes));
        }
        HostCommand::Update => {
            hosts::send_info_update(config).await?;
            println!("Host info update sent");
        }
        HostCommand::Metrics => {
            let metrics = HostMetrics::collect()?;
            println!("Used cpu:       {:>10} %", metrics.used_cpu_count);
            println!(
                "Used mem:       {:>10.3} GB",
                to_gb(metrics.used_memory_bytes)
            );
            println!(
                "Used disk:      {:>10.3} GB",
                to_gb(metrics.used_disk_space_bytes)
            );
            println!("Load (1 min):   {:>10}", metrics.load_one);
            println!("Load (5 mins):  {:>10}", metrics.load_five);
            println!("Load (15 mins): {:>10}", metrics.load_fifteen);
            println!(
                "Network in:     {:>10.3} GB",
                to_gb(metrics.network_received_bytes)
            );
            println!(
                "Network out:    {:>10.3} GB",
                to_gb(metrics.network_sent_bytes)
            );
            println!("Uptime:         {:>10} seconds", metrics.uptime_secs);
        }
    }

    Ok(())
}

#[allow(unreachable_code)]
async fn process_chain_command(command: ChainCommand) -> Result<()> {
    let bv_root = bv_root();
    let config = SharedConfig::new(Config::load(&bv_root).await?, bv_root);

    match command {
        ChainCommand::List {
            protocol,
            r#type,
            number,
        } => {
            let mut cookbook_service = CookbookService::connect(&config).await?;
            let mut versions = cookbook_service.list_versions(&protocol, &r#type).await?;

            versions.truncate(number);

            for version in versions {
                println!("{version}");
            }
        }
    }

    Ok(())
}

async fn process_workspace_command(command: WorkspaceCommand) -> Result<()> {
    match command {
        WorkspaceCommand::Create { path } => {
            workspace::create(&std::env::current_dir()?.join(path))?
        }
    }
    Ok(())
}

struct NodeClient {
    client: internal_server::service_client::ServiceClient<Channel>,
}

impl NodeClient {
    async fn new(url: String) -> Result<Self> {
        Ok(Self {
            client: internal_server::service_client::ServiceClient::connect(url).await?,
        })
    }

    async fn fetch_nodes(&mut self) -> Result<Vec<NodeDisplayInfo>> {
        Ok(self.client.get_nodes(()).await?.into_inner())
    }

    async fn list_capabilities(&mut self, node_id: Uuid) -> Result<Vec<String>> {
        Ok(self.client.list_capabilities(node_id).await?.into_inner())
    }

    async fn resolve_id_or_name(&mut self, id_or_name: &str) -> Result<Uuid> {
        let uuid = match Uuid::parse_str(id_or_name) {
            Ok(v) => v,
            Err(_) => {
                let id = self
                    .client
                    .get_node_id_for_name(id_or_name.to_string())
                    .await?
                    .into_inner();
                Uuid::parse_str(&id)?
            }
        };
        Ok(uuid)
    }

    async fn get_node_ids(&mut self, id_or_names: Vec<String>) -> Result<Vec<Uuid>> {
        let mut ids: Vec<Uuid> = Default::default();
        if id_or_names.is_empty() {
            for node in self.fetch_nodes().await? {
                ids.push(node.id);
            }
        } else {
            for id_or_name in id_or_names {
                let id = self.resolve_id_or_name(&id_or_name).await?;
                ids.push(id);
            }
        };
        Ok(ids)
    }

    async fn start_nodes(&mut self, ids: &[Uuid]) -> Result<()> {
        for id in ids {
            self.client.start_node(*id).await?;
            println!("Started node `{id}`");
        }
        Ok(())
    }

    async fn stop_nodes(&mut self, ids: &[Uuid]) -> Result<()> {
        for id in ids {
            self.client.stop_node(*id).await?;
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
                    .filter(|n| (!running || (n.status == NodeStatus::Running)))
                    .peekable();
                if nodes.peek().is_some() {
                    let mut table = vec![];
                    for node in nodes.cloned() {
                        table.push(PrettyTableRow {
                            id: node.id.to_string(),
                            name: node.name,
                            image: node.image.to_string(),
                            status: node.status,
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
                network,
            } => {
                let id = Uuid::new_v4();
                let name = Petnames::default().generate_one(3, "_");
                let image = parse_image(&image)?;
                let props: HashMap<String, String> = props
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or_default();
                let properties = props
                    .into_iter()
                    .chain([("network".to_string(), network.clone())])
                    .collect();
                let config = NodeConfig {
                    name: name.clone(),
                    image: image.clone(),
                    ip,
                    gateway,
                    properties,
                    network,
                    rules: vec![],
                };
                self.client.create_node((id, config)).await?;
                println!("Created new node from `{image}` image with ID `{id}` and name `{name}`");
                let _ = workspace::set_active_node(&std::env::current_dir()?, id, &name);
            }
            NodeCommand::Upgrade { id_or_names, image } => {
                let image = parse_image(&image)?;
                for id_or_name in node_ids_with_fallback(id_or_names, true)? {
                    let id = self.resolve_id_or_name(&id_or_name).await?;
                    self.client.upgrade_node((id, image.clone())).await?;
                    println!("Upgraded node `{id}` to `{image}` image");
                }
            }
            NodeCommand::Start { id_or_names } => {
                let ids = self
                    .get_node_ids(node_ids_with_fallback(id_or_names, false)?)
                    .await?;
                self.start_nodes(&ids).await?;
            }
            NodeCommand::Stop { id_or_names } => {
                let ids = self
                    .get_node_ids(node_ids_with_fallback(id_or_names, false)?)
                    .await?;
                self.stop_nodes(&ids).await?;
            }
            NodeCommand::Delete {
                id_or_names,
                all,
                yes,
            } => {
                let mut id_or_names = node_ids_with_fallback(id_or_names, false)?;
                // We only respect the `--all` flag when `id_or_names` is empty, in order to
                // prevent a typo from accidentally deleting all nodes.
                if id_or_names.is_empty() {
                    if all {
                        if ask_confirm("Are you sure you want to delete all nodes?", yes)? {
                            id_or_names = self
                                .fetch_nodes()
                                .await?
                                .into_iter()
                                .map(|n| n.id.to_string())
                                .collect();
                        } else {
                            return Ok(());
                        }
                    } else {
                        bail!("<ID_OR_NAMES> neither provided nor found in the workspace");
                    }
                }
                for id_or_name in id_or_names {
                    let id = self.resolve_id_or_name(&id_or_name).await?;
                    self.client.delete_node(id).await?;
                    println!("Deleted node `{id_or_name}`");
                }
            }
            NodeCommand::Restart { id_or_names } => {
                let ids = self
                    .get_node_ids(node_ids_with_fallback(id_or_names, true)?)
                    .await?;
                self.stop_nodes(&ids).await?;
                self.start_nodes(&ids).await?;
            }
            NodeCommand::Jobs { id_or_name } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                let jobs = self.client.get_node_jobs(id).await?.into_inner();
                if !jobs.is_empty() {
                    println!("{:<30} STATUS", "NAME");
                    for (name, status) in jobs {
                        let (exit_code, message) = match &status {
                            JobStatus::Pending => (None, None),
                            JobStatus::Running => (None, None),
                            JobStatus::Finished { exit_code, message } => {
                                (exit_code.map(|c| c as u64), Some(message.clone()))
                            }
                            JobStatus::Stopped => (None, None),
                        };
                        let status_with_code = format!(
                            "{:?}{}",
                            status,
                            exit_code.map(|c| format!("({c})")).unwrap_or_default()
                        );
                        println!(
                            "{:<30} {:<20} {}",
                            name,
                            status_with_code,
                            message.unwrap_or_default()
                        );
                    }
                }
            }
            NodeCommand::Logs { id_or_name } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                let logs = self.client.get_node_logs(id).await?;
                for log in logs.into_inner() {
                    print!("{log}");
                }
            }
            NodeCommand::BabelLogs {
                id_or_name,
                max_lines,
            } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                let logs = self.client.get_babel_logs((id, max_lines)).await?;
                for log in logs.into_inner() {
                    print!("{log}");
                }
            }
            NodeCommand::Status { id_or_names } => {
                for id_or_name in node_ids_with_fallback(id_or_names, true)? {
                    let id = self.resolve_id_or_name(&id_or_name).await?;
                    let status = self.client.get_node_status(id).await?;
                    let status = status.into_inner();
                    println!("{status}");
                }
            }
            NodeCommand::Keys { id_or_name } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                let keys = self.client.get_node_keys(id).await?;
                for name in keys.into_inner() {
                    println!("{name}");
                }
            }
            NodeCommand::Capabilities { id_or_name } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                let caps = self.list_capabilities(id).await?;
                for cap in caps {
                    println!("{cap}");
                }
            }
            NodeCommand::Run {
                id_or_name,
                method,
                param,
                param_file,
            } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                let param = match param {
                    Some(param) => param,
                    None => {
                        if let Some(path) = param_file {
                            fs::read_to_string(path)?
                        } else {
                            Default::default()
                        }
                    }
                };
                match self.client.run((id, method, param)).await {
                    Ok(result) => println!("{}", result.into_inner()),
                    Err(e) => {
                        if e.code() == Code::NotFound {
                            let msg = "Method not found. Options are:";
                            let caps = self
                                .list_capabilities(id)
                                .await?
                                .into_iter()
                                .reduce(|acc, cap| acc + "\n" + cap.as_str())
                                .unwrap_or_default();
                            bail!("{msg}\n{caps}");
                        }
                        return Err(anyhow::Error::from(e));
                    }
                }
            }
            NodeCommand::Metrics { id_or_name } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                let metrics = self.client.get_node_metrics(id).await?.into_inner();
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
            NodeCommand::Check { id_or_name } => {
                let id = self
                    .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                    .await?;
                // prepare list of checks
                // first go methods which SHALL and SHOULD be implemented
                let mut methods = vec![
                    "height",
                    "block_age",
                    "name",
                    "address",
                    "consensus",
                    "staking_status",
                    "sync_status",
                    "application_status",
                ];
                // second go test_* methods
                let caps = self.list_capabilities(id).await?;
                let tests_iter = caps
                    .iter()
                    .filter(|cap| cap.starts_with("test_"))
                    .map(|cap| cap.as_str());
                methods.extend(tests_iter);

                let mut errors = vec![];
                println!("Running node checks:");
                for method in methods {
                    let result = match self
                        .client
                        .run((id, method.to_string(), String::from("")))
                        .await
                    {
                        Ok(_) => "ok",
                        Err(e)
                            if e.code() == Code::NotFound || e.message().contains("not found") =>
                        {
                            // this is not considered an error
                            // and will not influence exit code
                            "not found"
                        }
                        Err(e) => {
                            errors.push(e);
                            "failed"
                        }
                    };
                    println!("{:.<30}{:.>16}", method, result);
                }
                if !errors.is_empty() {
                    eprintln!("\nGot {} errors:", errors.len());
                    for e in errors.iter() {
                        eprintln!("{e:#}");
                    }
                    bail!("Node check failed");
                }
            }
        }
        Ok(())
    }
}

fn fmt_opt<T: std::fmt::Debug>(opt: Option<T>) -> String {
    opt.map(|t| format!("{t:?}"))
        .unwrap_or_else(|| "-".to_string())
}

fn parse_image(image: &str) -> Result<NodeImage> {
    let image_vec: Vec<&str> = image.split('/').collect();
    if image_vec.len() != 3 {
        bail!("Wrong number of components in image: {image:?}");
    }
    Ok(NodeImage {
        protocol: image_vec[0].to_string(),
        node_type: image_vec[1].to_string(),
        node_version: image_vec[2].to_string(),
    })
}

fn node_id_with_fallback(node_id: Option<String>) -> Result<String> {
    Ok(match node_id {
        None => {
            if let Ok(workspace::Workspace {
                active_node: Some(workspace::ActiveNode { id, .. }),
                ..
            }) = workspace::read(&std::env::current_dir()?)
            {
                id.to_string()
            } else {
                bail!("<ID_OR_NAME> neither provided nor found in the workspace");
            }
        }
        Some(id) => id,
    })
}

fn node_ids_with_fallback(mut node_ids: Vec<String>, required: bool) -> Result<Vec<String>> {
    if node_ids.is_empty() {
        if let Ok(workspace::Workspace {
            active_node: Some(workspace::ActiveNode { id, .. }),
            ..
        }) = workspace::read(&std::env::current_dir()?)
        {
            node_ids.push(id.to_string());
        } else if required {
            bail!("<ID_OR_NAMES> neither provided nor found in the workspace");
        }
    }
    Ok(node_ids)
}
