use babel::{babel_service, logging, run_flag::RunFlag};
use eyre::Context;
use tonic::transport::Server;
use tracing::info;

const VSOCK_HOST_CID: u32 = 3;
const VSOCK_BABEL_PORT: u32 = 42;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    logging::setup_logging()?;
    info!(
        "Starting {} {} ...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let run = RunFlag::run_until_ctrlc();
    serve(run).await?;

    Ok(())
}

#[cfg(target_os = "linux")]
async fn serve(mut run: RunFlag) -> eyre::Result<()> {
    let babel_service = babel_service::BabelService::new()?;
    let listener = tokio_vsock::VsockListener::bind(VSOCK_HOST_CID, VSOCK_BABEL_PORT)
        .with_context(|| "failed to bind to vsock")?;

    Server::builder()
        .max_concurrent_streams(2)
        .add_service(babel_api::babel_server::BabelServer::new(babel_service))
        .serve_with_incoming_shutdown(listener.incoming(), run.wait())
        .await?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn serve(_run: RunFlag) -> eyre::Result<()> {
    unimplemented!()
}
