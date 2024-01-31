use blockvisord::{
    bv,
    cli::{App, Command},
    config::{Config, SharedConfig, CONFIG_PATH},
    internal_server,
    linux_platform::bv_root,
};
use bv_utils::cmd::run_cmd;
use clap::Parser;
use eyre::{bail, Result};
use tokio::time::{sleep, Duration};

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
            if let Ok(info) = service_info(bv_url.clone()).await {
                println!("Service already running: {info}");
                return Ok(());
            }

            run_cmd("systemctl", ["start", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_START_TIMEOUT).await;

            match service_info(bv_url.clone()).await {
                Ok(info) => println!("blockvisor service started successfully: {info}"),
                Err(e) => bail!("blockvisor service did not start: {e:#}"),
            }
        }
        Command::Stop(_) => {
            run_cmd("systemctl", ["stop", "blockvisor.service"]).await?;
            sleep(BLOCKVISOR_STOP_TIMEOUT).await;

            if let Ok(info) = service_info(bv_url).await {
                bail!("blockvisor service did not stop: {info}");
            } else {
                println!("blockvisor service stopped successfully");
            }
        }
        Command::Status(_) => {
            if let Ok(info) = service_info(bv_url).await {
                println!("Service running: {info}");
            } else {
                println!("Service stopped");
            }
        }
        Command::Host { command } => bv::process_host_command(config, command).await?,
        Command::Chain { command } => bv::process_chain_command(config, command).await?,
        Command::Node { command } => bv::process_node_command(bv_url, command).await?,
        Command::Workspace { command } => bv::process_workspace_command(bv_url, command).await?,
        Command::Image { command } => bv::process_image_command(bv_url, config, command).await?,
        Command::Cluster { command } => bv::process_cluster_command(bv_url, command).await?,
    }

    Ok(())
}

async fn service_info(url: String) -> Result<String> {
    let mut client = internal_server::service_client::ServiceClient::connect(url).await?;
    Ok(client.info(()).await?.into_inner())
}
