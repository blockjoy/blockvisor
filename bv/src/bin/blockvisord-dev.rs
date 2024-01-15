use blockvisord::{
    config::{Config, SharedConfig},
    internal_server,
    linux_platform::LinuxPlatform,
    nodes_manager::NodesManager,
    pal::Pal,
    set_bv_status, ServiceStatus,
};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use eyre::Result;
use std::sync::Arc;
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

    let mut run = RunFlag::run_until_ctrlc();
    let pal = LinuxPlatform::new()?;

    let bv_root = pal.bv_root().to_path_buf();
    let config = Config::load(&bv_root).await?;
    let listener = TcpListener::bind(format!("0.0.0.0:{}", config.blockvisor_port)).await?;

    let config = SharedConfig::new(config, bv_root);
    let nodes = NodesManager::load(pal, config.clone()).await?;
    let nodes = Arc::new(nodes);

    Server::builder()
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
        .await?;

    info!("Stopping...");
    Ok(())
}
