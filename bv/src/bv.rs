use crate::cli::JobCommand;
use crate::{
    cli::{ChainCommand, HostCommand, ImageCommand, NodeCommand, WorkspaceCommand},
    config::{Config, SharedConfig},
    hosts::{self, HostInfo, HostMetrics},
    internal_server,
    linux_platform::bv_root,
    node::REGISTRY_CONFIG_DIR,
    node_data::{NodeImage, NodeStatus},
    nodes::NodeConfig,
    pretty_table::{PrettyTable, PrettyTableRow},
    services::cookbook::{
        CookbookService, BABEL_ARCHIVE_IMAGE_NAME, BABEL_PLUGIN_NAME, IMAGES_DIR, ROOT_FS_FILE,
    },
    workspace, BV_VAR_PATH,
};
use babel_api::engine::JobStatus;
use bv_utils::cmd::{ask_confirm, run_cmd};
use cli_table::print_stdout;
use eyre::{anyhow, bail, Context, Result};
use petname::Petnames;
use std::future::Future;
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};
use tonic::{transport::Channel, Code};
use uuid::Uuid;

pub async fn process_host_command(config: SharedConfig, command: HostCommand) -> Result<()> {
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

pub async fn process_node_command(bv_url: String, command: NodeCommand) -> Result<()> {
    let mut client = NodeClient::new(bv_url).await?;
    match command {
        NodeCommand::List { running } => {
            let nodes = client.get_nodes(()).await?.into_inner();
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
            let image = parse_image(&image_id_with_fallback(image)?)?;
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
            client.client.create_node((id, config)).await?;
            println!("Created new node from `{image}` image with ID `{id}` and name `{name}`");
            let _ = workspace::set_active_node(&std::env::current_dir()?, id, &name);
        }
        NodeCommand::Upgrade { id_or_names, image } => {
            let image = parse_image(&image)?;
            for id_or_name in node_ids_with_fallback(id_or_names, true)? {
                let id = client.resolve_id_or_name(&id_or_name).await?;
                client.client.upgrade_node((id, image.clone())).await?;
                println!("Upgraded node `{id}` to `{image}` image");
            }
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
                            println!("{name:<30} {status:?}", status = info.status);
                        }
                    }
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
                JobCommand::Info { name } => {
                    let info = client.get_node_job_info((id, name)).await?.into_inner();
                    println!("{info:#?}");
                }
            }
        }
        NodeCommand::Logs { id_or_name } => {
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                .await?;
            let logs = client.get_node_logs(id).await?;
            for log in logs.into_inner() {
                print!("{log}");
            }
        }
        NodeCommand::BabelLogs {
            id_or_name,
            max_lines,
        } => {
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                .await?;
            let logs = client.get_babel_logs((id, max_lines)).await?;
            for log in logs.into_inner() {
                print!("{log}");
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
        NodeCommand::Keys { id_or_name } => {
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                .await?;
            let keys = client.get_node_keys(id).await?;
            for name in keys.into_inner() {
                println!("{name}");
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
        NodeCommand::Metrics { id_or_name } => {
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(id_or_name)?)
                .await?;
            let metrics = client.get_node_metrics(id).await?.into_inner();
            println!("Block height:   {:>12}", fmt_opt(metrics.height));
            println!("Block age:      {:>12}", fmt_opt(metrics.block_age));
            println!("Staking Status: {:>12}", fmt_opt(metrics.staking_status));
            println!("In consensus:   {:>12}", fmt_opt(metrics.consensus));
            println!(
                "App Status:     {:>12}",
                fmt_opt(metrics.application_status)
            );
            println!("Sync Status:    {:>12}", fmt_opt(metrics.sync_status));
            if !metrics.jobs.is_empty() {
                println!("Jobs:");
                for (name, mut info) in metrics.jobs {
                    println!("  - \"{name}\"");
                    println!("    Status: {:?}", info.status);
                    println!("    Restarts: {}", info.restart_count);
                    if let Some(progress) = info.progress {
                        println!(
                            "    Progress: {}/{} {}",
                            progress.current, progress.total, progress.message
                        );
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
        }
        NodeCommand::Check { id_or_name } => {
            let id = client
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
            let caps = client.list_capabilities(id).await?.into_inner();
            let tests_iter = caps
                .iter()
                .filter(|cap| cap.starts_with("test_"))
                .map(|cap| cap.as_str());
            methods.extend(tests_iter);

            let mut errors = vec![];
            println!("Running node checks:");
            for method in methods {
                let result = match client.run((id, method.to_string(), String::from(""))).await {
                    Ok(_) => "ok",
                    Err(e) if e.code() == Code::NotFound || e.message().contains("not found") => {
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

pub async fn process_image_command(bv_url: String, command: ImageCommand) -> Result<()> {
    match command {
        ImageCommand::Create {
            image_id,
            debian_version,
            rootfs_size,
        } => {
            let destination_image = parse_image(&image_id)?;
            let images_dir = build_bv_var_path().join(IMAGES_DIR);
            let destination_image_path = images_dir.join(image_id);
            fs::create_dir_all(&destination_image_path)?;
            render_rhai_file(&destination_image_path, &destination_image).await?;
            bootstrap_os_image(
                &destination_image_path,
                &destination_image,
                &debian_version,
                rootfs_size,
            )
            .await?;
            let _ = workspace::set_active_image(&std::env::current_dir()?, destination_image);
        }
        ImageCommand::Clone {
            source_image_id,
            destination_image_id,
        } => {
            parse_image(&source_image_id)?; // just validate source image id format
            let destination_image = parse_image(&destination_image_id)?;
            let images_dir = build_bv_var_path().join(IMAGES_DIR);
            let destination_image_path = images_dir.join(destination_image_id);
            fs::create_dir_all(&destination_image_path)?;
            fs_extra::dir::copy(
                images_dir.join(source_image_id),
                &destination_image_path,
                &fs_extra::dir::CopyOptions::default().content_only(true),
            )?;
            update_babelsup(&destination_image_path, &destination_image).await?;
            let _ = workspace::set_active_image(&std::env::current_dir()?, destination_image);
        }
        ImageCommand::Capture { node_id_or_name } => {
            let mut client = NodeClient::new(bv_url).await?;
            let id = client
                .resolve_id_or_name(&node_id_with_fallback(node_id_or_name)?)
                .await?;
            let node = client.get_node(id).await?.into_inner();
            if NodeStatus::Stopped != node.status {
                if ask_confirm(
                    "Node must be stopped before capture! Do you want to stop it now?",
                    false,
                )? {
                    client.stop_node((id, true)).await?;
                } else {
                    bail!("Can't capture running node!")
                }
            }
            let build_bv_var_path = build_bv_var_path();
            let image_dir = build_bv_var_path
                .join(IMAGES_DIR)
                .join(format!("{}", node.image));
            // capture rhai script
            fs::copy(
                build_bv_var_path
                    .join(REGISTRY_CONFIG_DIR)
                    .join(format!("{id}.rhai")),
                image_dir.join(BABEL_PLUGIN_NAME),
            )?;
            // capture os.img
            fs::copy(
                build_bv_var_path
                    .join(format!("firecracker/{id}/root"))
                    .join(ROOT_FS_FILE),
                image_dir.join(ROOT_FS_FILE),
            )?;
            cleanup_rootfs(&image_dir, &node.image)
                .await
                .with_context(|| "failed to cleanup rootfs")?;
        }
        ImageCommand::Upload {
            image_id,
            s3_endpoint,
            s3_region,
            s3_bucket,
            s3_prefix,
        } => {
            let image_id = image_id_with_fallback(image_id)?;
            parse_image(&image_id)?; // just validate source image id format
            let s3_client = S3Client::new(s3_endpoint, s3_region, s3_bucket, s3_prefix, image_id)?;
            s3_client.upload_file(BABEL_PLUGIN_NAME).await?;
            s3_client
                .archive_and_upload_file(ROOT_FS_FILE, BABEL_ARCHIVE_IMAGE_NAME)
                .await?;
        }
    }
    Ok(())
}

pub async fn process_chain_command(command: ChainCommand) -> Result<()> {
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
            workspace::set_active_node(&current_dir, id, &node.name)?;
        }
        WorkspaceCommand::SetActiveImage { image_id } => {
            workspace::set_active_image(&current_dir, parse_image(&image_id)?)?;
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
            client: internal_server::service_client::ServiceClient::connect(url).await?,
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

    async fn stop_nodes(&mut self, ids: &[Uuid], force: bool) -> Result<()> {
        for id in ids {
            self.client.stop_node((*id, force)).await?;
            println!("Stopped node `{id}`");
        }
        Ok(())
    }
}

struct S3Client {
    client: aws_sdk_s3::Client,
    s3_bucket: String,
    s3_prefix: String,
    image_dir: PathBuf,
}

impl S3Client {
    fn new(
        s3_endpoint: String,
        s3_region: String,
        s3_bucket: String,
        s3_prefix: String,
        image_id: String,
    ) -> Result<Self> {
        Ok(Self {
            client: aws_sdk_s3::Client::from_conf(
                aws_sdk_s3::Config::builder()
                    .endpoint_url(s3_endpoint)
                    .region(aws_sdk_s3::config::Region::new(s3_region))
                    .credentials_provider(aws_sdk_s3::config::Credentials::new(
                        std::env::var("AWS_ACCESS_KEY_ID")?,
                        std::env::var("AWS_SECRET_ACCESS_KEY")?,
                        None,
                        None,
                        "Custom Provided Credentials",
                    ))
                    .build(),
            ),
            s3_bucket,
            s3_prefix: format!("{s3_prefix}/{image_id}"),
            image_dir: build_bv_var_path().join(IMAGES_DIR).join(image_id),
        })
    }

    async fn upload_file(&self, file_name: &str) -> Result<()> {
        println!(
            "Uploading {file_name} to {}/{}/{} ...",
            self.s3_bucket, self.s3_prefix, file_name
        );
        let file_path = self.image_dir.join(file_name);
        self.client
            .put_object()
            .bucket(&self.s3_bucket)
            .key(&format!("{}/{}", self.s3_prefix, file_name))
            .set_content_length(Some(i64::try_from(file_path.metadata()?.len())?))
            .body(aws_sdk_s3::primitives::ByteStream::from_path(file_path).await?)
            .send()
            .await?;
        Ok(())
    }

    async fn archive_and_upload_file(
        &self,
        file_name: &str,
        archive_file_name: &str,
    ) -> Result<()> {
        println!("Archiving {file_name} ...");
        let mut file_path = self.image_dir.join(file_name).into_os_string();
        let archive_file_path = &self.image_dir.join(archive_file_name);
        run_cmd("gzip", [OsStr::new("-kf"), &file_path]).await?;
        file_path.push(".gz");
        fs::rename(file_path, archive_file_path)?;
        self.upload_file(archive_file_name).await?;
        fs::remove_file(archive_file_path)?;
        Ok(())
    }
}

fn build_bv_var_path() -> PathBuf {
    bv_root().join(BV_VAR_PATH)
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

fn image_id_with_fallback(image: Option<String>) -> Result<String> {
    Ok(match image {
        None => {
            if let Ok(workspace::Workspace {
                active_image: Some(image),
                ..
            }) = workspace::read(&std::env::current_dir()?)
            {
                format!("{image}")
            } else {
                bail!("<IMAGE_ID> neither provided nor found in the workspace");
            }
        }
        Some(image_id) => image_id,
    })
}

async fn bootstrap_os_image(
    image_path: &Path,
    image: &NodeImage,
    debian_version: &str,
    rootfs_size_gb: usize,
) -> Result<()> {
    let os_img_path = image_path.join(ROOT_FS_FILE);

    println!("Creating the disk image with fallocate");
    let gb = &format!("{rootfs_size_gb}GB");
    run_cmd(
        "fallocate",
        [OsStr::new("-l"), OsStr::new(gb), os_img_path.as_os_str()],
    )
    .await?;

    println!("Creating the file system on this disk image");
    run_cmd("mkfs.ext4", [os_img_path.as_os_str()]).await?;

    on_rootfs(image_path, image, |mount_point| async move {
        install_os_and_packages(debian_version, &mount_point).await?;
        extract_babelsup(&mount_point).await
    })
    .await
}

async fn install_os_and_packages(debian_version: &str, mount_point: &Path) -> Result<()> {
    println!("Debootstrapping `{debian_version}`");
    run_cmd(
        "debootstrap",
        [OsStr::new(debian_version), mount_point.as_os_str()],
    )
    .await?;

    println!("Installing some basics in chroot");
    run_in_chroot(
        mount_point,
        "apt install -y software-properties-common wget curl uuid-runtime",
    )
    .await?;

    println!("Adding repositories in chroot");
    run_in_chroot(mount_point, "add-apt-repository -y universe").await?;

    println!("Updating OS packages in chroot");
    run_in_chroot(mount_point, "apt update -y").await?;

    println!("Installing some important pacckages");
    run_in_chroot(
        mount_point,
        "apt install -y build-essential libssl-dev jq ufw",
    )
    .await?;

    Ok(())
}

async fn run_in_chroot(mount_point: &Path, cmd: &str) -> Result<()> {
    run_cmd(
        "chroot",
        [
            mount_point.as_os_str(),
            OsStr::new("bash"),
            OsStr::new("-c"),
            OsStr::new(cmd),
        ],
    )
    .await?;

    Ok(())
}

async fn render_rhai_file(image_path: &Path, image: &NodeImage) -> Result<()> {
    let mut context = tera::Context::new();
    context.insert("protocol", &image.protocol);
    context.insert("node_type", &image.node_type);
    let mut tera = tera::Tera::default();
    let template = include_str!("../data/babel.rhai.template");
    tera.add_raw_template("template", template)?;
    let rhai_file_path = image_path.join(BABEL_PLUGIN_NAME);
    println!("Render rhai file at `{}`", rhai_file_path.display());
    let out_file = fs::File::create(rhai_file_path)?;
    tera.render_to("template", &context, out_file)?;
    Ok(())
}

async fn update_babelsup(image_path: &Path, image: &NodeImage) -> Result<()> {
    on_rootfs(image_path, image, |mount_point| async move {
        extract_babelsup(&mount_point).await
    })
    .await
}

async fn extract_babelsup(target_dir: &Path) -> Result<()> {
    let babelsup_path = fs::canonicalize(
        std::env::current_exe().with_context(|| "failed to get current binary path")?,
    )
    .with_context(|| "non canonical current binary path")?
    .parent()
    .ok_or_else(|| anyhow!("invalid current binary dir - has no parent"))?
    .join("../../babelsup.tar.gz");
    run_cmd(
        "tar",
        [
            OsStr::new("--no-same-owner"),
            OsStr::new("--no-same-permissions"),
            OsStr::new("-C"),
            target_dir.as_os_str(),
            OsStr::new("-xf"),
            babelsup_path.as_os_str(),
        ],
    )
    .await?;
    Ok(())
}

/// Cleanup rootfs from babel state remnants (remove /var/lib/babel).
async fn cleanup_rootfs(image_path: &Path, image: &NodeImage) -> Result<()> {
    on_rootfs(image_path, image, |mount_point| async move {
        let babel_dir = mount_point.join("var/lib/babel");
        if babel_dir.exists() {
            fs::remove_dir_all(babel_dir)?;
        }
        cleanup_ignored(&mount_point)
    })
    .await
}

fn cleanup_ignored(bv_root: &Path) -> Result<()> {
    let bv_ignore_path = bv_root.join("etc/bvignore");
    if bv_ignore_path.exists() {
        for pattern in fs::read_to_string(bv_ignore_path)?
            .split('\n')
            .filter(|pattern| !pattern.is_empty())
        {
            for entry in nu_glob::glob(&bv_root.join(pattern).to_string_lossy())? {
                let path = entry?;
                if path.exists() {
                    if path.is_dir() {
                        fs::remove_dir_all(&path).ok();
                    } else if path.is_file() {
                        fs::remove_file(&path).ok();
                    }
                }
            }
        }
    }
    Ok(())
}

async fn on_rootfs<C: FnOnce(PathBuf) -> F, F: Future<Output = Result<()>>>(
    image_path: &Path,
    image: &NodeImage,
    call: C,
) -> Result<()> {
    let os_img_path = image_path.join(ROOT_FS_FILE);
    let mount_point = std::env::temp_dir().join(format!(
        "{}_{}_{}_rootfs",
        image.protocol, image.node_type, image.node_version
    ));
    fs::create_dir_all(&mount_point)?;

    run_cmd("mount", [os_img_path.as_os_str(), mount_point.as_os_str()])
        .await
        .with_context(|| "failed to mount os.img")?;
    let call_result = call(mount_point.clone()).await;
    run_cmd("umount", [mount_point.as_os_str()])
        .await
        .with_context(|| "failed to umount os.img")?;
    call_result
}
