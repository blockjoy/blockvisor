use crate::babel_engine::NodeInfo;
use crate::{
    config::SharedConfig,
    firecracker_machine::VSOCK_PATH,
    services::{self, api::pb},
};
use async_trait::async_trait;
use babel_api::engine::DownloadManifest;
use bv_utils::with_retry;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{
    transport::Server,
    {Request, Response, Status},
};
use tracing::error;

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
        let manifest = request.into_inner();
        // DownloadManifest may be pretty big, so better set longer timeout that depends on number of chunks
        let custom_timeout =
            Duration::from_secs(5 + u64::try_from(manifest.chunks.len() / 1000).unwrap_or(10));
        let manifest = pb::BlockchainArchiveServicePutDownloadManifestRequest {
            id: Some(self.node_info.image.clone().try_into().map_err(|err| {
                Status::invalid_argument(format!("invalid node image id: {err}"))
            })?),
            network: self.node_info.network.clone(),
            manifest: Some(manifest.try_into().map_err(|err| {
                Status::invalid_argument(format!("invalid manifest blueprint: {err}"))
            })?),
        };
        let build_request = || {
            let mut req = Request::new(manifest.clone());
            req.set_timeout(custom_timeout);
            req
        };
        let mut archive_service = services::connect_to_api_service(
            &self.config,
            pb::blockchain_archive_service_client::BlockchainArchiveServiceClient::with_interceptor,
        )
        .await
        .map_err(|err| Status::internal(format!("can not connect archives service: {err}")))?;
        with_retry!(archive_service.put_download_manifest(build_request()))
            .map_err(|err| Status::internal(format!("put_download_manifest failed with: {err}")))?;
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
