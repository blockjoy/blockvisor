use crate::{
    babel_engine::NodeInfo, config::SharedConfig, firecracker_machine::VSOCK_PATH, services,
};
use async_trait::async_trait;
use babel_api::engine::DownloadManifest;
use std::path::PathBuf;
use tokio::fs;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{
    transport::Server,
    {Request, Response, Status},
};
use tracing::{debug, error};

const BABEL_ENGINE_PORT: u32 = 40;

struct BabelEngineService {
    node_info: NodeInfo,
    config: SharedConfig,
}

#[async_trait]
impl babel_api::babel::babel_engine_server::BabelEngine for BabelEngineService {
    async fn put_download_manifest(
        &self,
        request: Request<DownloadManifest>,
    ) -> eyre::Result<Response<()>, Status> {
        debug!("putting DownloadManifest to API...");
        let manifest = request.into_inner();
        services::blockchain_archive::put_download_manifest(
            &self.config,
            self.node_info.image.clone(),
            self.node_info.network.clone(),
            manifest,
        )
        .await
        .map_err(|err| Status::internal(err.to_string()))?;
        Ok(Response::new(()))
    }

    async fn upgrade_blocking_jobs_finished(
        &self,
        _request: Request<()>,
    ) -> eyre::Result<Response<()>, Status> {
        Ok(Response::new(()))
    }

    async fn bv_error(&self, request: Request<String>) -> eyre::Result<Response<()>, Status> {
        let message = request.into_inner();
        error!("Babel: {message}");
        Ok(Response::new(()))
    }
}

#[derive(Debug)]
pub struct BabelEngineServer {
    handle: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    tx: tokio::sync::oneshot::Sender<()>,
}

pub async fn start_server(
    vm_data_path: PathBuf,
    node_info: NodeInfo,
    config: SharedConfig,
) -> eyre::Result<BabelEngineServer> {
    let socket_path = vm_data_path.join(format!("{VSOCK_PATH}_{BABEL_ENGINE_PORT}"));
    let engine_service = BabelEngineService { node_info, config };
    let _ = fs::remove_file(&socket_path).await;
    let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(socket_path)?);
    let (tx, rx) = tokio::sync::oneshot::channel();
    Ok(BabelEngineServer {
        handle: tokio::spawn(
            Server::builder()
                .max_concurrent_streams(1)
                .add_service(
                    babel_api::babel::babel_engine_server::BabelEngineServer::new(engine_service),
                )
                .serve_with_incoming_shutdown(uds_stream, async {
                    rx.await.ok();
                }),
        ),
        tx,
    })
}

impl BabelEngineServer {
    pub async fn stop(self) -> eyre::Result<()> {
        let _ = self.tx.send(());
        Ok(self.handle.await??)
    }
}
