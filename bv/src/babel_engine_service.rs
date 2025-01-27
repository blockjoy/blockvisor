use crate::{babel_engine::NodeInfo, bv_config::SharedConfig, services};
use async_trait::async_trait;
use babel_api::engine::{Chunk, DownloadManifest, DownloadMetadata, UploadSlots};
use eyre::Context;
use std::path::PathBuf;
use tokio::fs;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{
    transport::Server,
    {Request, Response, Status},
};
use tracing::{debug, error, warn};

struct BabelEngineService {
    node_info: NodeInfo,
    config: SharedConfig,
}

#[async_trait]
impl babel_api::babel::babel_engine_server::BabelEngine for BabelEngineService {
    async fn put_download_manifest(
        &self,
        request: Request<(DownloadManifest, u64)>,
    ) -> eyre::Result<Response<()>, Status> {
        let (manifest, data_version) = request.into_inner();
        debug!(
            "putting DownloadManifest to API from node {} (archive_id={}/{})",
            self.node_info.node_id, self.node_info.image.archive_id, data_version,
        );
        if let Err(err) = services::archive::put_download_manifest(
            &self.config,
            self.node_info.image.archive_id.clone(),
            manifest,
            data_version,
        )
        .await
        .with_context(|| {
            format!(
                "put_download_manifest for node {} (archive_id={})",
                self.node_info.node_id, self.node_info.image.archive_id
            )
        }) {
            warn!("{err:#}");
            Err(Status::internal(err.to_string()))
        } else {
            Ok(Response::new(()))
        }
    }

    async fn get_download_metadata(
        &self,
        _request: Request<()>,
    ) -> eyre::Result<Response<DownloadMetadata>, Status> {
        debug!(
            "getting DownloadMetadata from API for node {} (archive_id={})",
            self.node_info.node_id, self.node_info.image.archive_id
        );
        match services::archive::get_download_metadata(
            &self.config,
            self.node_info.image.archive_id.clone(),
        )
        .await
        .with_context(|| {
            format!(
                "get_download_manifest for node {} (archive_id={})",
                self.node_info.node_id, self.node_info.image.archive_id
            )
        }) {
            Err(err) => {
                warn!("{err:#}");
                Err(Status::internal(err.to_string()))
            }
            Ok(metadata) => Ok(Response::new(metadata)),
        }
    }
    async fn get_download_chunks(
        &self,
        request: Request<(u64, Vec<u32>)>,
    ) -> eyre::Result<Response<Vec<Chunk>>, Status> {
        let (data_version, chunk_indexes) = request.into_inner();
        debug!(
            "getting DownloadChunks from API for node {} (archive_id={}/{})",
            self.node_info.node_id, self.node_info.image.archive_id, data_version,
        );
        match services::archive::get_download_chunks(
            &self.config,
            self.node_info.image.archive_id.clone(),
            data_version,
            chunk_indexes,
        )
        .await
        {
            Err(err) => {
                warn!("{err:#}");
                Err(Status::internal(err.to_string()))
            }
            Ok(chunks) => Ok(Response::new(chunks)),
        }
    }

    async fn get_upload_slots(
        &self,
        request: Request<(Option<u64>, Vec<u32>, u32)>,
    ) -> eyre::Result<Response<UploadSlots>, Status> {
        let (data_version, slots, url_expires_secs) = request.into_inner();
        let slots_count = slots.len();
        debug!(
            "getting UploadSlots from API for node {} (archive_id={}/{:?}, slots={})",
            self.node_info.node_id, self.node_info.image.archive_id, data_version, slots_count
        );
        match services::archive::get_upload_slots(
            &self.config,
            self.node_info.image.archive_id.clone(),
            data_version,
            slots,
            url_expires_secs,
        )
        .await
        .with_context(|| {
            format!(
                "get_upload_slots for node {} (archive_id={}/{:?}, slots={})",
                self.node_info.node_id, self.node_info.image.archive_id, data_version, slots_count
            )
        }) {
            Err(err) => {
                warn!("{err:#}");
                Err(Status::internal(err.to_string()))
            }
            Ok(slots) => Ok(Response::new(slots)),
        }
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
    engine_socket_path: PathBuf,
    node_info: NodeInfo,
    config: SharedConfig,
) -> eyre::Result<BabelEngineServer> {
    let engine_service = BabelEngineService { node_info, config };
    let _ = fs::remove_file(&engine_socket_path).await;
    let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(engine_socket_path)?);
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
