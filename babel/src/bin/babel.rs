use babel::{
    babel_service, babel_service::JobRunnerLock, jobs::JOBS_DIR, jobs_manager, logging, utils,
};
use bv_utils::run_flag::RunFlag;
use eyre::Context;
use std::{path::Path, sync::Arc};
use tokio::sync::RwLock;
use tonic::transport::Server;
use tracing::info;

lazy_static::lazy_static! {
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

    let (client, manager) =
        jobs_manager::create(&JOBS_DIR, job_runner_lock.clone(), &JOB_RUNNER_BIN_PATH)?;

    let mut run = RunFlag::run_until_ctrlc();
    let manager_handle = tokio::spawn(manager.run(run.clone()));
    let res = serve(run.clone(), job_runner_lock, client).await;
    if run.load() {
        // make sure to stop manager gracefully
        // in case of abnormal server shutdown
        run.stop();
        manager_handle.await?;
    }
    res
}

async fn serve(
    mut run: RunFlag,
    job_runner_lock: JobRunnerLock,
    jobs_manager: jobs_manager::Client,
) -> eyre::Result<()> {
    let babel_service = babel_service::BabelService::new(
        job_runner_lock,
        JOB_RUNNER_BIN_PATH.to_path_buf(),
        jobs_manager,
    )?;
    let listener = tokio_vsock::VsockListener::bind(VSOCK_HOST_CID, VSOCK_BABEL_PORT)
        .with_context(|| "failed to bind to vsock")?;

    Server::builder()
        .max_concurrent_streams(2)
        .add_service(babel_api::babel_server::BabelServer::new(babel_service))
        .serve_with_incoming_shutdown(listener.incoming(), run.wait())
        .await?;
    Ok(())
}
