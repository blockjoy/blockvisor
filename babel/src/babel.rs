use crate::pal::BabelServer;
use crate::{
    babel_service, is_babel_config_applied, jobs::JOBS_DIR, jobs_manager,
    jobs_manager::JobsManagerState, load_config, pal, utils, JOBS_MONITOR_UDS_PATH,
};
use bv_utils::run_flag::RunFlag;
use std::{path::Path, sync::Arc};
use tokio::{fs, sync::RwLock};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

lazy_static::lazy_static! {
    static ref JOB_RUNNER_BIN_PATH: &'static Path = Path::new("/usr/bin/babel_job_runner");
    static ref BABEL_CONFIG_PATH: &'static Path = Path::new("/etc/babel.conf");
}

pub async fn run<P>(pal: P) -> eyre::Result<()>
where
    P: pal::BabelPal + Send + Sync + 'static,
    P::Connector: Send + Sync + 'static,
{
    let babel_server = pal.babel_server();
    let job_runner_lock = Arc::new(RwLock::new(
        utils::file_checksum(&JOB_RUNNER_BIN_PATH).await.ok(),
    ));

    let jobs_manager_state = if let Ok(config) = load_config(&BABEL_CONFIG_PATH).await {
        if is_babel_config_applied(&pal, &config).await? {
            JobsManagerState::Ready
        } else {
            JobsManagerState::NotReady
        }
    } else {
        JobsManagerState::NotReady
    };

    let (client, monitor, manager) = jobs_manager::create(
        pal.connector(),
        &JOBS_DIR,
        job_runner_lock.clone(),
        &JOB_RUNNER_BIN_PATH,
        jobs_manager_state,
    )
    .await?;
    let babel_service = babel_service::BabelService::new(
        job_runner_lock,
        JOB_RUNNER_BIN_PATH.to_path_buf(),
        client,
        BABEL_CONFIG_PATH.to_path_buf(),
        pal,
    )?;

    let mut run = RunFlag::run_until_ctrlc();
    let manager_handle = tokio::spawn(manager.run(run.clone()));

    let monitor_run = run.clone();
    let (res, _) = tokio::join!(
        babel_server.serve(
            babel_api::babel::babel_server::BabelServer::new(babel_service),
            run.clone()
        ),
        serve_jobs_monitor::<P>(monitor_run, monitor)
    );
    if run.load() {
        // make sure to stop manager gracefully
        // in case of abnormal server shutdown
        run.stop();
    }
    manager_handle.await?;
    res
}

async fn serve_jobs_monitor<P>(
    mut run: RunFlag,
    jobs_monitor_service: jobs_manager::Monitor,
) -> eyre::Result<()>
where
    P: pal::BabelPal + Send + Sync,
    P::Connector: Send + Sync + 'static,
{
    let _ = fs::remove_file(JOBS_MONITOR_UDS_PATH).await;
    let uds_stream =
        UnixListenerStream::new(tokio::net::UnixListener::bind(JOBS_MONITOR_UDS_PATH)?);

    Server::builder()
        .add_service(
            babel_api::babel::jobs_monitor_server::JobsMonitorServer::new(jobs_monitor_service),
        )
        .serve_with_incoming_shutdown(uds_stream, run.wait())
        .await?;
    Ok(())
}
