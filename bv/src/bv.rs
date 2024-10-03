use crate::node_state::ProtocolImageKey;
use crate::{
    apptainer_machine::ROOTFS_DIR,
    bv_cli::{
        ClusterCommand, HostCommand, JobCommand, NodeCommand, ProtocolCommand, WorkspaceCommand,
    },
    bv_config::SharedConfig,
    hosts::{self, HostInfo},
    internal_server,
    internal_server::CreateNodeRequest,
    linux_platform::bv_root,
    node_context::build_node_dir,
    node_env::NODE_ENV_FILE_PATH,
    node_state::VmStatus,
    pretty_table::{PrettyTable, PrettyTableRow},
    services,
    services::protocol::ProtocolService,
    workspace,
};
use babel_api::engine::JobStatus;
use bv_utils::cmd::ask_confirm;
use bv_utils::rpc::RPC_CONNECT_TIMEOUT;
use chrono::Utc;
use cli_table::print_stdout;
use eyre::{bail, Result};
use std::ffi::OsString;
use std::{
    ffi::OsStr,
    fs,
    ops::{Deref, DerefMut},
};
use tokio::process::Command;
use tonic::transport::Endpoint;
use tonic::{transport::Channel, Code};
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
            println!("Used cpu:       {:>10} %", metrics.used_cpu_count);
            println!(
                "Used mem:       {:>10.3} GB",
                to_gb(metrics.used_memory_bytes)
            );
            println!(
                "Used disk:      {:>10.3} GB",
                to_gb(metrics.used_disk_space_bytes)
            );
            println!("Used IPs:      {:?}", metrics.used_ips);
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

pub async fn process_node_command(bv_url: String, command: NodeCommand) -> Result<()> {
    let mut client = NodeClient::new(bv_url).await?;
    match command {
        NodeCommand::List { running } => {
            let nodes = client.get_nodes(()).await?.into_inner();
            let mut nodes = nodes
                .iter()
                .filter(|n| !running || n.status == VmStatus::Running)
                .peekable();
            if nodes.peek().is_some() {
                let mut table = vec![];
                for node in nodes.cloned() {
                    table.push(PrettyTableRow {
                        id: node.state.id.to_string(),
                        name: node.state.name,
                        triple: format!(
                            "{}/{}/{}",
                            node.state.image_key.protocol_key,
                            node.state.image_key.variant_key,
                            node.state.image.version
                        ),
                        status: node.status,
                        ip: node.state.ip.to_string(),
                        uptime: fmt_opt(
                            node.state
                                .started_at
                                .map(|dt| Utc::now().signed_duration_since(dt).num_seconds()),
                        ),
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
                })
                .await?
                .into_inner();
            println!(
                "Created new node with ID `{}` and name `{}`\n{:#?}",
                node.state.id, node.state.name, node.state
            );
            let _ = workspace::set_active_node(
                &std::env::current_dir()?,
                node.state.id,
                &node.state.name,
            );
        }
        NodeCommand::Start { id_or_names } => {
            let ids = client
                .get_node_ids(node_ids_with_fallback(id_or_names, false)?)
                .await?;
            client.start_nodes(&ids).await?;
        }
        NodeCommand::Stop { id_or_names, force } => {
            let ids = client
                .get_node_ids(node_ids_with_fallback(id_or_names, false)?)
                .await?;
            client.stop_nodes(&ids, force).await?;
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
                        id_or_names = client
                            .get_nodes(())
                            .await?
                            .into_inner()
                            .into_iter()
                            .map(|n| n.state.id.to_string())
                            .collect();
                    } else {
                        return Ok(());
                    }
                } else {
                    bail!("<ID_OR_NAMES> neither provided nor found in the workspace");
                }
            } else if !ask_confirm(
                &format!("Are you sure you want to delete following node(s)?\n{id_or_names:?}"),
                yes,
            )? {
                return Ok(());
            }
            for id_or_name in id_or_names {
                let id = client.resolve_id_or_name(&id_or_name).await?;
                client.delete_node(id).await?;
                let _ = workspace::unset_active_node(&std::env::current_dir()?, id);
                println!("Deleted node `{id_or_name}`");
            }
        }
        NodeCommand::Restart { id_or_names, force } => {
            let ids = client
                .get_node_ids(node_ids_with_fallback(id_or_names, true)?)
                .await?;
            client.stop_nodes(&ids, force).await?;
            client.start_nodes(&ids).await?;
        }
        NodeCommand::Job {
            command,
            id_or_name,
        } => {
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                .await?;
            match command {
                JobCommand::List => {
                    let jobs = client.get_node_jobs(id).await?.into_inner();
                    if !jobs.is_empty() {
                        println!("{:<30} STATUS", "NAME");
                        for (name, info) in jobs {
                            println!("{name:<30} {status}", status = info.status);
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

                    println!("status:           {}", info.status);
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
                    let logs_path = build_node_dir(&bv_root(), id)
                        .join(ROOTFS_DIR)
                        .join("var/lib/babel/jobs/logs");
                    if let Some(name) = &name {
                        if !logs_path.join(name).exists() {
                            bail!("No logs for '{name}' job found!")
                        }
                    }
                    let mut cmd = Command::new("tail");
                    cmd.current_dir(&logs_path);
                    cmd.args(["-n", &format!("{lines}")]);
                    if follow {
                        cmd.arg("-f");
                    }
                    let names = if let Some(name) = name {
                        vec![OsString::from(name)]
                    } else {
                        fs::read_dir(logs_path)?
                            .map(|entry| entry.map(|entry| entry.file_name()))
                            .collect::<Result<Vec<_>, std::io::Error>>()?
                    };
                    cmd.args(names);
                    cmd.spawn()?.wait().await?;
                }
            }
        }
        NodeCommand::Status { id_or_names } => {
            for id_or_name in node_ids_with_fallback(id_or_names, true)? {
                let id = client.resolve_id_or_name(&id_or_name).await?;
                let status = client.get_node_status(id).await?;
                let status = status.into_inner();
                println!("{status}");
            }
        }
        NodeCommand::Capabilities { id_or_name } => {
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                .await?;
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
            let id = client
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
        NodeCommand::Check { id_or_name } => {
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                .await?;
            let node_info = client.get_node(id).await?.into_inner();
            println!("Name:           {}", node_info.state.name);
            println!("Id:             {}", node_info.state.id);
            println!("Image:          {:?}", node_info.state.image);
            println!("Status:         {}", node_info.status);
            println!("Ip:             {}", node_info.state.ip);
            println!("Gateway:        {}", node_info.state.gateway);
            println!(
                "Uptime:         {}s",
                fmt_opt(
                    node_info
                        .state
                        .started_at
                        .map(|dt| Utc::now().signed_duration_since(dt).num_seconds())
                )
            );
            let metrics = client.get_node_metrics(id).await?.into_inner();
            println!("App Status:     {}", fmt_opt(metrics.application_status));
            println!("Block height:   {}", fmt_opt(metrics.height));
            println!("Block age:      {}", fmt_opt(metrics.block_age));
            println!("In consensus:   {}", fmt_opt(metrics.consensus));
            if !metrics.jobs.is_empty() {
                println!("Jobs:");
                for (name, mut info) in metrics.jobs {
                    println!("  - \"{name}\"");
                    println!("    Status:         {}", info.status);
                    println!("    Restarts:       {}", info.restart_count);
                    if let Some(progress) = info.progress {
                        println!("    Progress:       {progress}");
                    }
                    if !info.logs.is_empty() {
                        if info.logs.len() > 7 {
                            let _ = info.logs.split_off(6);
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
            // run plugin linter and test_* methods if in dev_mode
            if node_info.state.dev_mode {
                // TODO MJR move to bib
                // let node_context = NodeContext::build(&bv_root(), id);
                // let (script, _) = node_context
                //     .load_script(&node_context.node_dir.join(ROOTFS_DIR))
                //     .await?;
                // println!(
                //     "Plugin linter: {:?}",
                //     rhai_plugin_linter::check(&script, node_info.properties)
                // );
                // let caps = client.list_capabilities(id).await?.into_inner();
                // let tests = caps
                //     .iter()
                //     .filter(|cap| cap.starts_with("test_"))
                //     .map(|cap| cap.as_str())
                //     .collect::<Vec<_>>();
                // if !tests.is_empty() {
                //     let mut errors = vec![];
                //     println!("Running node internal tests:");
                //     for test in tests {
                //         let result =
                //             match client.run((id, test.to_string(), String::default())).await {
                //                 Ok(_) => "ok",
                //                 Err(e) => {
                //                     errors.push(e);
                //                     "failed"
                //                 }
                //             };
                //         println!("{:.<30}{:.>16}", test, result);
                //     }
                //     if !errors.is_empty() {
                //         eprintln!("\n{} tests failed:", errors.len());
                //         for e in errors.iter() {
                //             eprintln!("{e:#}");
                //         }
                //         bail!("Node internal tests failed");
                //     }
                // }
            }
        }
        NodeCommand::Shell { id_or_name } => {
            let (name, id) = match id_or_name {
                None => {
                    if let Ok(workspace::Workspace {
                        active_node: Some(workspace::ActiveNode { name, id }),
                        ..
                    }) = workspace::read(&std::env::current_dir()?)
                    {
                        (name, id)
                    } else {
                        bail!("<ID_OR_NAME> neither provided nor found in the workspace");
                    }
                }
                Some(id_or_name) => match Uuid::parse_str(&id_or_name) {
                    Ok(id) => (client.get_node(id).await?.into_inner().state.name, id),
                    Err(_) => (
                        id_or_name.clone(),
                        Uuid::parse_str(
                            &client.get_node_id_for_name(id_or_name).await?.into_inner(),
                        )?,
                    ),
                },
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
            cmd.arg(format!("instance://{name}"));
            cmd.spawn()?.wait().await?;
        }
    }
    Ok(())
}

pub async fn process_protocol_command(
    config: SharedConfig,
    command: ProtocolCommand,
) -> Result<()> {
    match command {
        ProtocolCommand::List { protocol, number } => {
            let mut protocol_service =
                ProtocolService::new(services::DefaultConnector { config }).await?;
            let mut versions = protocol_service.list_protocol_images(&protocol).await?;

            versions.truncate(number);

            for version in versions {
                println!("{version}");
            }
        }
    }

    Ok(())
}

pub async fn process_workspace_command(bv_url: String, command: WorkspaceCommand) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    match command {
        WorkspaceCommand::Create { path } => {
            workspace::create(&current_dir.join(path))?;
        }
        WorkspaceCommand::SetActiveNode { id_or_name } => {
            let mut client = NodeClient::new(bv_url).await?;
            let id = client.resolve_id_or_name(&id_or_name).await?;
            let node = client.get_node(id).await?.into_inner();
            workspace::set_active_node(&current_dir, id, &node.state.name)?;
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
            for node in self.get_nodes(()).await?.into_inner() {
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

fn fmt_opt<T: std::fmt::Debug>(opt: Option<T>) -> String {
    opt.map(|t| format!("{t:?}"))
        .unwrap_or_else(|| "-".to_string())
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
