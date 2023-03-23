use babel::babel_service::JobRunnerLock;
use babel::{babel_service, logging, utils};
use bv_utils::run_flag::RunFlag;
use eyre::Context;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::transport::Server;
use tracing::info;

lazy_static::lazy_static! {
    static ref JOBS_PATH: &'static Path = Path::new("/var/lib/babel/jobs");
    static ref JOB_RUNNER_BIN_PATH: &'static Path = Path::new("/usr/bin/babel_job_runner");
}
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

    let job_runner_lock = Arc::new(RwLock::new(
        utils::file_checksum(&JOB_RUNNER_BIN_PATH).await.ok(),
    ));

    let run = RunFlag::run_until_ctrlc();
    serve(run, job_runner_lock).await?;

    Ok(())
}

async fn serve(mut run: RunFlag, job_runner_lock: JobRunnerLock) -> eyre::Result<()> {
    let babel_service =
        babel_service::BabelService::new(job_runner_lock, JOB_RUNNER_BIN_PATH.to_path_buf())?;
    let listener = tokio_vsock::VsockListener::bind(VSOCK_HOST_CID, VSOCK_BABEL_PORT)
        .with_context(|| "failed to bind to vsock")?;

    Server::builder()
        .max_concurrent_streams(2)
        .add_service(babel_api::babel_server::BabelServer::new(babel_service))
        .serve_with_incoming_shutdown(listener.incoming(), run.wait())
        .await?;
    Ok(())
}
