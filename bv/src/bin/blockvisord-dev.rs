use anyhow::Result;
use blockvisord::{
    config::{Config, SharedConfig},
    linux_platform::LinuxPlatform,
    nodes::Nodes,
    pal::Pal,
    server::{bv_pb, BlockvisorServer},
};
use bv_utils::{logging::setup_logging, run_flag::RunFlag};
use std::sync::Arc;
use tokio::net::TcpListener;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging();
    info!(
        "Starting {} {} ...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let mut run = RunFlag::run_until_ctrlc();
    let pal = LinuxPlatform::new()?;

    let bv_root = pal.bv_root().to_path_buf();
    let config = Config::load(&bv_root).await?;
    let listener = TcpListener::bind(format!("0.0.0.0:{}", config.blockvisor_port)).await?;

    let nodes = Nodes::load(pal, SharedConfig::new(config, bv_root)).await?;

    Server::builder()
        .max_concurrent_streams(1)
        .add_service(bv_pb::blockvisor_server::BlockvisorServer::new(
            BlockvisorServer {
                nodes: Arc::new(nodes),
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
