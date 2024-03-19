use blockvisord::{
    config, config::SharedConfig, internal_server, nodes_manager::NodesManager, pal::Pal,
    pal_config, set_bv_status, ServiceStatus,
};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::Result;
use std::{fmt::Debug, sync::Arc};
use tokio::net::TcpListener;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    set_bv_status(ServiceStatus::Ok).await;

    match pal_config::PalConfig::load().await? {
        pal_config::PalConfig::LinuxFc => {
            let pal = blockvisord::linux_fc_platform::LinuxFcPlatform::new().await?;
            let config = config::Config::load(pal.bv_root()).await?;
            run_server(config, pal).await?;
        }
        pal_config::PalConfig::LinuxBare => {
            let pal = blockvisord::linux_bare_platform::LinuxBarePlatform::new().await?;
            let config = config::Config::load(pal.bv_root()).await?;
            run_server(config, pal).await?;
        }
    }

    info!("Stopping...");
    Ok(())
}

async fn run_server<P>(config: config::Config, pal: P) -> Result<()>
where
    P: Pal + Debug + Send + Sync + 'static,
    P::NetInterface: Send + Sync + 'static,
    P::NodeConnection: Send + Sync + 'static,
    P::ApiServiceConnector: Send + Sync + 'static,
    P::VirtualMachine: Send + Sync + 'static,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    let mut run = RunFlag::run_until_ctrlc();
    let bv_root = pal.bv_root().to_path_buf();
    let listener = TcpListener::bind(format!("0.0.0.0:{}", config.blockvisor_port)).await?;

    let config = SharedConfig::new(config, bv_root);
    let nodes = NodesManager::load(pal, config.clone()).await?;
    let nodes = Arc::new(nodes);

    Ok(Server::builder()
        .max_concurrent_streams(1)
        .add_service(internal_server::service_server::ServiceServer::new(
            internal_server::State {
                config,
                nodes_manager: nodes,
                cluster: Arc::new(None),
                dev_mode: true,
            },
        ))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            run.wait(),
        )
        .await?)
}
