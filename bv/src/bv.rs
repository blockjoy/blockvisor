use crate::{
    apptainer_machine::ROOTFS_DIR,
    bv_cli::{ClusterCommand, HostCommand, JobCommand, NodeCommand, ProtocolCommand},
    bv_config::SharedConfig,
    hosts::{self, HostInfo},
    internal_server,
    internal_server::CreateNodeRequest,
    linux_platform::bv_root,
    node_context::build_node_dir,
    node_env::NODE_ENV_FILE_PATH,
    node_state::{ProtocolImageKey, VmStatus},
    pretty_table::{PrettyTable, PrettyTableRow},
    services,
    services::protocol::ProtocolService,
};
use babel_api::engine::JobStatus;
use bv_utils::{cmd::ask_confirm, rpc::RPC_CONNECT_TIMEOUT};
use chrono::{DateTime, Utc};
use cli_table::print_stdout;
use eyre::{bail, Result};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    ops::{Deref, DerefMut},
};
use tokio::process::Command;
use tonic::{
    transport::{Channel, Endpoint},
    Code,
};
use uuid::Uuid;

pub async fn process_host_command(
    bv_url: String,
    config: SharedConfig,
    command: HostCommand,
) -> Result<()> {
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
            let mut client = NodeClient::new(bv_url).await?;
            let metrics = client.get_host_metrics(()).await?.into_inner();
            println!("Used cpu:       {:>10} %", metrics.used_cpu);
            println!(
                "Used mem:       {:>10.3} GB",
                to_gb(metrics.used_memory_bytes)
            );
            println!(
                "Used disk:      {:>10.3} GB",
                to_gb(metrics.used_disk_space_bytes)
            );
            println!("Used IPs:        {:?}", metrics.used_ips);
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
            println!("Uptime [h:m:s]: {:>13}", fmt_seconds(metrics.uptime_secs));
        }
    }

    Ok(())
}

pub async fn process_node_command(bv_url: String, command: NodeCommand) -> Result<()> {
    let mut client = NodeClient::new(bv_url).await?;
    match command {
        NodeCommand::FixLegacy { path } => {
            let mapping: HashMap<String, (String, ProtocolImageKey)> =
                serde_json::from_str(&fs::read_to_string(path)?)?;
            client.fix_legacy_nodes(mapping).await?;
        }
        NodeCommand::List {
            running,
            local,
            tags,
        } => {
            let mut nodes = client.get_nodes(local).await?.into_inner();
            if running {
                nodes.retain(|n| n.status == VmStatus::Running);
            }
            if !tags.is_empty() {
                nodes.retain(|n| n.state.tags.iter().any(|tag| tags.contains(tag)));
            }
            if !nodes.is_empty() {
                let mut table = vec![];
                for node in nodes {
                    table.push(PrettyTableRow {
                        id: node.state.id.to_string(),
                        name: node.state.name,
                        image: format!(
                            "{}/{}/{}",
                            node.state.image_key.protocol_key,
                            node.state.image_key.variant_key,
                            node.state.image.version
                        ),
                        status: node.status,
                        ip: node.state.ip.to_string(),
                        uptime: fmt_opt(node.state.started_at.map(fmt_uptime)),
                    })
                }
                print_stdout(table.to_pretty_table())?;
            } else {
                println!("No nodes found.");
            }
        }
        NodeCommand::Create {
            protocol,
            variant,
            version,
            build,
            props,
            tags,
        } => {
            let properties = if let Some(props) = props {
                serde_json::from_str(&props)?
            } else {
                Default::default()
            };
            let node = client
                .client
                .create_node(CreateNodeRequest {
                    protocol_image_key: ProtocolImageKey {
                        protocol_key: protocol,
                        variant_key: variant,
                    },
                    image_version: version,
                    build_version: build,
                    properties,
                    tags,
                })
                .await?
                .into_inner();
            println!(
                "Created new node with ID `{}` and name `{}`\n{:#?}",
                node.state.id, node.state.name, node.state
            );
        }
        NodeCommand::Start { id_or_names } => {
            let ids = client.get_node_ids(id_or_names).await?;
            client.start_nodes(&ids).await?;
        }
        NodeCommand::Stop { id_or_names, force } => {
            let ids = client.get_node_ids(id_or_names).await?;
            client.stop_nodes(&ids, force).await?;
        }
        NodeCommand::Restart { id_or_names, force } => {
            if id_or_names.is_empty() {
                bail!("<ID_OR_NAMES> can't be empty list");
            }
            let ids = client.get_node_ids(id_or_names).await?;
            client.stop_nodes(&ids, force).await?;
            client.start_nodes(&ids).await?;
        }
        NodeCommand::Upgrade {
            mut id_or_names,
            version,
            build,
            all,
        } => {
            if id_or_names.is_empty() {
                if all {
                    id_or_names = client
                        .get_nodes(true)
                        .await?
                        .into_inner()
                        .into_iter()
                        .map(|n| n.state.id.to_string())
                        .collect();
                } else {
                    bail!("<ID_OR_NAMES> can't be empty list");
                }
            }
            let ids = client.get_node_ids(id_or_names).await?;
            for id in ids {
                if let Err(err) = client.upgrade_node((id, version.clone(), build)).await {
                    println!("Failed to upgrade node `{id}`: {err:#}");
                } else {
                    println!("Node `{id}` upgrade triggered");
                }
            }
        }
        NodeCommand::Delete {
            mut id_or_names,
            tags,
            all,
            yes,
        } => {
            if !id_or_names.is_empty() {
                if !ask_confirm(
                    &format!("Are you sure you want to delete following node(s)?\n{id_or_names:?}"),
                    yes,
                )? {
                    return Ok(());
                }
            } else if !tags.is_empty() {
                if !ask_confirm(
                    &format!("Are you sure you want to delete following node(s)?\n{id_or_names:?}"),
                    yes,
                )? {
                    return Ok(());
                }
            } else if all {
                if ask_confirm("Are you sure you want to delete all nodes?", yes)? {
                    id_or_names = client
                        .get_nodes(true)
                        .await?
                        .into_inner()
                        .into_iter()
                        .map(|n| n.state.id.to_string())
                        .collect();
                } else {
                    return Ok(());
                }
            }
            for id_or_name in id_or_names {
                let id = client.resolve_id_or_name(&id_or_name).await?;
                client.delete_node(id).await?;
                println!("Deleted node `{id_or_name}`");
            }
        }
        NodeCommand::Job {
            command,
            id_or_name,
        } => {
            let id = client.resolve_id_or_name(&id_or_name).await?;
            match command {
                JobCommand::List => {
                    let jobs = client.get_node_jobs(id).await?.into_inner();
                    if !jobs.is_empty() {
                        println!("{:<20} STATUS", "NAME");
                        for (name, info) in jobs {
                            println!("{name:<20} {status}", status = info.status,);
                        }
                    }
                }
                JobCommand::Start { name } => {
                    client.start_node_job((id, name)).await?;
                }
                JobCommand::Stop { name, .. } => {
                    if let Some(name) = name {
                        client.stop_node_job((id, name)).await?;
                    } else {
                        for (name, info) in client.get_node_jobs(id).await?.into_inner() {
                            if JobStatus::Running == info.status {
                                client.stop_node_job((id, name)).await?;
                            }
                        }
                    }
                }
                JobCommand::Skip { name } => {
                    client.skip_node_job((id, name)).await?;
                }
                JobCommand::Cleanup { name } => {
                    client.cleanup_node_job((id, name)).await?;
                }
                JobCommand::Info { name } => {
                    let info = client.get_node_job_info((id, name)).await?.into_inner();
                    let timestamp: DateTime<Utc> = info.timestamp.into();

                    println!(
                        "status:           {}| {}",
                        timestamp.format("%F %T %Z"),
                        info.status,
                    );
                    if let Some(progress) = info.progress {
                        println!("progress:         {progress}",);
                    }
                    println!("restart_count:    {}", info.restart_count);
                    println!("upgrade_blocking: {}", info.upgrade_blocking);
                    print!("logs:             ");
                    if info.logs.is_empty() {
                        println!("<empty>");
                    } else {
                        println!();
                        for log in info.logs {
                            println!("{log}")
                        }
                    }
                }
                JobCommand::Logs {
                    name,
                    lines,
                    follow,
                } => {
                    let jobs_path = build_node_dir(&bv_root(), id)
                        .join(ROOTFS_DIR)
                        .join("var/lib/babel/jobs");
                    if let Some(name) = &name {
                        if !jobs_path.join(name).exists() {
                            bail!("No logs for '{name}' job found!")
                        }
                    }
                    let mut cmd = Command::new("tail");
                    cmd.current_dir(&jobs_path);
                    cmd.args(["-n", &format!("{lines}")]);
                    if follow {
                        cmd.arg("-f");
                    }
                    let jobs = if let Some(name) = name {
                        vec![name]
                    } else {
                        fs::read_dir(&jobs_path)?
                            .filter_map(|entry| match entry {
                                Ok(entry) => {
                                    if entry.path().is_dir() {
                                        Some(Ok(entry.file_name().to_string_lossy().to_string()))
                                    } else {
                                        None
                                    }
                                }
                                Err(err) => Some(Err(err)),
                            })
                            .collect::<Result<Vec<_>, std::io::Error>>()?
                    };
                    cmd.args(
                        jobs.into_iter()
                            .filter_map(|job_name| {
                                jobs_path
                                    .join(&job_name)
                                    .join("logs")
                                    .exists()
                                    .then(|| format!("{job_name}/logs"))
                            })
                            .collect::<Vec<_>>(),
                    );
                    cmd.spawn()?.wait().await?;
                }
            }
        }
        NodeCommand::Status { id_or_names } => {
            for id_or_name in id_or_names {
                let id = client.resolve_id_or_name(&id_or_name).await?;
                let status = client.get_node(id).await?.into_inner().status;
                println!("{status}");
            }
        }
        NodeCommand::Capabilities { id_or_name } => {
            let id = client.resolve_id_or_name(&id_or_name).await?;
            let caps = client.list_capabilities(id).await?.into_inner();
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
            let id = client.resolve_id_or_name(&id_or_name).await?;
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
            match client.run((id, method, param)).await {
                Ok(result) => println!("{}", result.into_inner()),
                Err(e) => {
                    if e.code() == Code::NotFound {
                        let msg = "Method not found. Options are:";
                        let caps = client
                            .list_capabilities(id)
                            .await?
                            .into_inner()
                            .into_iter()
                            .reduce(|acc, cap| acc + "\n" + cap.as_str())
                            .unwrap_or_default();
                        bail!("{msg}\n{caps}");
                    }
                    return Err(eyre::Error::from(e));
                }
            }
        }
        NodeCommand::Info { id_or_name } => {
            let id = client.resolve_id_or_name(&id_or_name).await?;
            let node_info = client.get_node(id).await?.into_inner();
            println!("Name:           {}", node_info.state.name);
            println!("Id:             {}", node_info.state.id);
            println!("Image:          {:?}", node_info.state.image);
            println!("Status:         {}", node_info.status);
            println!("Ip:             {}", node_info.state.ip);
            println!("Gateway:        {}", node_info.state.gateway);
            println!(
                "Uptime [h:m:s]: {}",
                fmt_opt(node_info.state.started_at.map(fmt_uptime))
            );
            let metrics = client.get_node_metrics(id).await?.into_inner();
            println!(
                "Protocol Status:     {}",
                fmt_opt(metrics.protocol_status.map(|status| format!("{status:?}")))
            );
            println!("Block height:   {}", fmt_opt(metrics.height));
            println!("Block age:      {}", fmt_opt(metrics.block_age));
            println!("In consensus:   {}", fmt_opt(metrics.consensus));
            if !metrics.jobs.is_empty() {
                println!("Jobs:");
                for (name, mut info) in metrics.jobs {
                    println!("  - \"{name}\"");
                    let timestamp: DateTime<Utc> = info.timestamp.into();
                    println!(
                        "    Status:         {}| {}",
                        timestamp.format("%F %T %Z"),
                        info.status,
                    );
                    println!("    Restarts:       {}", info.restart_count);
                    if let Some(progress) = info.progress {
                        println!("    Progress:       {progress}");
                    }
                    if !info.logs.is_empty() {
                        if info.logs.len() > 3 {
                            info.logs.truncate(3);
                            info.logs
                                .push(format!("... use `bv node job info {}` to get more", name));
                        }
                        println!("    Logs:");
                        for log in info.logs {
                            println!("      {}", log);
                        }
                    }
                }
            }
            println!(
                "Requirements:   cpu: {}, memory: {}MB, disk: {}GB",
                node_info.state.vm_config.vcpu_count,
                node_info.state.vm_config.mem_size_mb,
                node_info.state.vm_config.disk_size_gb
            );
            println!("Assigned CPUs:  {:?}", node_info.state.assigned_cpus);
            println!("Tags:  {:?}", node_info.state.tags);
        }
        NodeCommand::Shell { id_or_name } => {
            let id = match Uuid::parse_str(&id_or_name) {
                Ok(id) => id,
                Err(_) => {
                    Uuid::parse_str(&client.get_node_id_for_name(id_or_name).await?.into_inner())?
                }
            };

            let mut cmd = Command::new("apptainer");
            cmd.args(["shell", "--ipc", "--cleanenv", "--userns", "--pid"]);
            cmd.args([
                OsStr::new("--env-file"),
                build_node_dir(&bv_root(), id)
                    .join(ROOTFS_DIR)
                    .join(NODE_ENV_FILE_PATH)
                    .as_os_str(),
            ]);
            cmd.arg(format!("instance://{id}"));
            cmd.spawn()?.wait().await?;
        }
        NodeCommand::ReloadPlugin { id_or_name } => {
            let id = match Uuid::parse_str(&id_or_name) {
                Ok(id) => id,
                Err(_) => {
                    Uuid::parse_str(&client.get_node_id_for_name(id_or_name).await?.into_inner())?
                }
            };
            client.reload_plugin(id).await?;
        }
    }
    Ok(())
}

pub async fn process_protocol_command(
    config: SharedConfig,
    command: ProtocolCommand,
) -> Result<()> {
    match command {
        ProtocolCommand::List { name, number } => {
            let mut protocol_service =
                ProtocolService::new(services::DefaultConnector { config }).await?;
            for protocol in protocol_service.list_protocols(name, number).await? {
                println!("{protocol}");
            }
        }
    }

    Ok(())
}

pub async fn process_cluster_command(bv_url: String, command: ClusterCommand) -> Result<()> {
    let mut client = NodeClient::new(bv_url).await?;

    match command {
        ClusterCommand::Status {} => {
            let status = client.get_cluster_status(()).await?.into_inner();
            // TODO: this just is a POC
            println!("{status}");
        }
    }

    Ok(())
}

struct NodeClient {
    client: internal_server::service_client::ServiceClient<Channel>,
}

impl Deref for NodeClient {
    type Target = internal_server::service_client::ServiceClient<Channel>;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl DerefMut for NodeClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

impl NodeClient {
    async fn new(url: String) -> Result<Self> {
        Ok(Self {
            client: internal_server::service_client::ServiceClient::connect(
                Endpoint::from_shared(url)?.connect_timeout(RPC_CONNECT_TIMEOUT),
            )
            .await?,
        })
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
            for node in self.get_nodes(true).await?.into_inner() {
                ids.push(node.state.id);
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

    async fn stop_nodes(&mut self, ids: &[Uuid], force: bool) -> Result<()> {
        for id in ids {
            self.client.stop_node((*id, force)).await?;
            println!("Stopped node `{id}`");
        }
        Ok(())
    }
}

fn fmt_opt<T: std::fmt::Display>(opt: Option<T>) -> String {
    opt.map(|t| format!("{t}"))
        .unwrap_or_else(|| "-".to_string())
}

fn fmt_uptime(uptime: DateTime<Utc>) -> String {
    fmt_seconds(Utc::now().signed_duration_since(uptime).abs().num_seconds() as u64)
}

fn fmt_seconds(mut seconds: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = MIN * 60;
    const DAY: u64 = HOUR * 24;
    let days = seconds / DAY;
    seconds -= days * DAY;
    let hours = seconds / HOUR;
    seconds -= hours * HOUR;
    let mins = seconds / MIN;
    seconds -= mins * MIN;
    let mut time = if days > 0 {
        format!("{days}d ")
    } else {
        String::new()
    };
    time.push_str(&format!("{hours:02}:{mins:02}:{seconds:02}"));
    time
}
