use crate::babelsup_service::pb::{start_new_babel_request, StartNewBabelRequest};
use crate::utils;
use async_trait::async_trait;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{broadcast, watch, Mutex};
use tonic::{Request, Response, Status, Streaming};

pub mod pb {
    #![allow(clippy::derive_partial_eq_without_eq)]
    tonic::include_proto!("blockjoy.babelsup.v1");
}

pub struct BabelSupService {
    logs_rx: Arc<Mutex<broadcast::Receiver<String>>>,
    babel_change_tx: watch::Sender<Option<u32>>,
    babel_bin_path: PathBuf,
}

#[async_trait]
impl pb::babel_sup_server::BabelSup for BabelSupService {
    type GetLogsStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<pb::GetLogsResponse, Status>>>;

    async fn get_logs(
        &self,
        _request: Request<pb::GetLogsRequest>,
    ) -> Result<Response<Self::GetLogsStream>, Status> {
        let mut logs = Vec::default();
        let mut rx = self.logs_rx.lock().await;
        loop {
            match rx.try_recv() {
                Ok(log) => logs.push(Ok(pb::GetLogsResponse { log })),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        let stream = tokio_stream::iter(logs);
        Ok(Response::new(stream))
    }

    async fn check_babel(
        &self,
        request: Request<pb::CheckBabelRequest>,
    ) -> Result<Response<pb::CheckBabelResponse>, Status> {
        let expected_checksum = request.into_inner().checksum;
        let babel_status = match *self.babel_change_tx.borrow() {
            Some(checksum) => {
                if checksum == expected_checksum {
                    pb::check_babel_response::BabelStatus::Ok
                } else {
                    pb::check_babel_response::BabelStatus::ChecksumMismatch
                }
            }
            None => pb::check_babel_response::BabelStatus::Missing,
        } as i32;
        Ok(Response::new(pb::CheckBabelResponse { babel_status }))
    }

    async fn start_new_babel(
        &self,
        request: Request<Streaming<pb::StartNewBabelRequest>>,
    ) -> Result<Response<pb::StartNewBabelResponse>, Status> {
        let mut stream = request.into_inner();
        let expected_checksum = self.save_babel_stream(&mut stream).await?;

        let checksum = utils::file_checksum(&self.babel_bin_path)
            .await
            .map_err(|err| {
                Status::internal(format!("failed to calculate babel binary checksum: {err}"))
            })?;

        if expected_checksum == checksum {
            self.babel_change_tx.send_modify(|value| {
                let _ = value.insert(checksum);
            });
            Ok(Response::new(pb::StartNewBabelResponse {}))
        } else {
            Err(Status::internal(format!(
                "received babel binary checksum ({checksum})\
                 doesn't match expected ({expected_checksum})"
            )))
        }
    }
}

impl BabelSupService {
    pub fn new(
        logs_rx: broadcast::Receiver<String>,
        babel_change_tx: watch::Sender<Option<u32>>,
        babel_bin_path: PathBuf,
    ) -> Self {
        Self {
            logs_rx: Arc::new(Mutex::new(logs_rx)),
            babel_change_tx,
            babel_bin_path,
        }
    }

    async fn save_babel_stream(
        &self,
        stream: &mut Streaming<StartNewBabelRequest>,
    ) -> Result<u32, Status> {
        let _ = tokio::fs::remove_file(&self.babel_bin_path).await;
        let file = OpenOptions::new()
            .write(true)
            .mode(0o770)
            .append(false)
            .create(true)
            .open(&self.babel_bin_path)
            .await
            .map_err(|err| Status::internal(format!("failed to open babel binary file: {err}")))?;
        let mut writer = BufWriter::new(file);
        let mut expected_checksum = None;
        while let Some(part) = stream.next().await {
            let part = part.map_err(|e| Status::internal(e.to_string()))?;
            match part.babel_bin {
                Some(start_new_babel_request::BabelBin::Bin(bin)) => {
                    writer.write(&bin).await.map_err(|err| {
                        Status::internal(format!("failed to save babel binary: {err}"))
                    })?;
                    Ok(())
                }
                Some(start_new_babel_request::BabelBin::Checksum(checksum)) => {
                    expected_checksum = Some(checksum);
                    Ok(())
                }
                _ => Err(Status::invalid_argument("incomplete babel_bin stream")),
            }?
        }
        writer
            .flush()
            .await
            .map_err(|err| Status::internal(format!("failed to save babel binary: {err}")))?;
        expected_checksum.ok_or_else(|| {
            Status::invalid_argument("incomplete babel_bin stream - missing checksum")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::babelsup_service::pb::babel_sup_client::BabelSupClient;
    use crate::babelsup_service::pb::babel_sup_server::BabelSup;
    use assert_fs::TempDir;
    use eyre::Result;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::{Channel, Endpoint, Server, Uri};

    async fn sup_server(
        tmp_root: &Path,
        logs_rx: broadcast::Receiver<String>,
        babel_change_tx: watch::Sender<Option<u32>>,
        uds_stream: UnixListenerStream,
    ) -> Result<()> {
        let sup_service = BabelSupService::new(logs_rx, babel_change_tx, tmp_root.join("babel"));
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::babel_sup_server::BabelSupServer::new(sup_service))
            .serve_with_incoming(uds_stream)
            .await?;
        Ok(())
    }

    fn test_client(tmp_root: &Path) -> Result<BabelSupClient<Channel>> {
        let socket_path = tmp_root.join("test_socket");
        let channel = Endpoint::try_from("http://[::]:50052")
            .unwrap()
            .timeout(Duration::from_secs(1))
            .connect_timeout(Duration::from_secs(1))
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                UnixStream::connect(socket_path.clone())
            }));
        Ok(BabelSupClient::new(channel))
    }

    struct TestEnv {
        babel_path: PathBuf,
        logs_tx: broadcast::Sender<String>,
        babel_change_rx: watch::Receiver<Option<u32>>,
        client: BabelSupClient<Channel>,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_root)?;
        let babel_path = tmp_root.join("babel");
        let client = test_client(&tmp_root)?;
        let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(
            tmp_root.join("test_socket"),
        )?);
        let (babel_change_tx, babel_change_rx) = watch::channel(None);
        let (logs_tx, logs_rx) = broadcast::channel(16);
        tokio::spawn(
            async move { sup_server(&tmp_root, logs_rx, babel_change_tx, uds_stream).await },
        );

        Ok(TestEnv {
            babel_path,
            logs_tx,
            babel_change_rx,
            client,
        })
    }

    #[tokio::test]
    async fn test_start_new_babel() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let incomplete_babel_bin = vec![
            pb::StartNewBabelRequest {
                babel_bin: Some(pb::start_new_babel_request::BabelBin::Bin(vec![
                    1, 2, 3, 4, 6, 7, 8, 9, 10,
                ])),
            },
            pb::StartNewBabelRequest {
                babel_bin: Some(pb::start_new_babel_request::BabelBin::Bin(vec![
                    11, 12, 13, 14, 16, 17, 18, 19, 20,
                ])),
            },
            pb::StartNewBabelRequest {
                babel_bin: Some(pb::start_new_babel_request::BabelBin::Bin(vec![
                    21, 22, 23, 24, 26, 27, 28, 29, 30,
                ])),
            },
        ];

        assert!(test_env
            .client
            .start_new_babel(tokio_stream::iter(incomplete_babel_bin.clone()))
            .await
            .is_err());
        assert!(!test_env.babel_change_rx.has_changed()?);

        let mut invalid_babel_bin = incomplete_babel_bin.clone();
        invalid_babel_bin.push(pb::StartNewBabelRequest {
            babel_bin: Some(pb::start_new_babel_request::BabelBin::Checksum(123)),
        });
        assert!(test_env
            .client
            .start_new_babel(tokio_stream::iter(invalid_babel_bin))
            .await
            .is_err());
        assert!(!test_env.babel_change_rx.has_changed()?);

        let mut babel_bin = incomplete_babel_bin.clone();
        babel_bin.push(pb::StartNewBabelRequest {
            babel_bin: Some(pb::start_new_babel_request::BabelBin::Checksum(4135829304)),
        });
        test_env
            .client
            .start_new_babel(tokio_stream::iter(babel_bin))
            .await?;
        assert!(test_env.babel_change_rx.has_changed()?);
        assert_eq!(4135829304, test_env.babel_change_rx.borrow().unwrap());
        assert_eq!(
            4135829304,
            utils::file_checksum(&test_env.babel_path).await.unwrap()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_check_babel() -> Result<()> {
        let babel_bin_path = TempDir::new().unwrap().join("babel");
        let (_, logs_rx) = broadcast::channel(16);

        let (babel_change_tx, _) = watch::channel(None);
        let sup_service = BabelSupService::new(
            logs_rx.resubscribe(),
            babel_change_tx,
            babel_bin_path.clone(),
        );

        assert_eq!(
            pb::check_babel_response::BabelStatus::Missing as i32,
            sup_service
                .check_babel(Request::new(pb::CheckBabelRequest { checksum: 123 }))
                .await?
                .into_inner()
                .babel_status
        );

        let (babel_change_tx, _) = watch::channel(Some(321));
        let sup_service = BabelSupService::new(
            logs_rx.resubscribe(),
            babel_change_tx,
            babel_bin_path.clone(),
        );

        assert_eq!(
            pb::check_babel_response::BabelStatus::ChecksumMismatch as i32,
            sup_service
                .check_babel(Request::new(pb::CheckBabelRequest { checksum: 123 }))
                .await?
                .into_inner()
                .babel_status
        );
        assert_eq!(
            pb::check_babel_response::BabelStatus::Ok as i32,
            sup_service
                .check_babel(Request::new(pb::CheckBabelRequest { checksum: 321 }))
                .await?
                .into_inner()
                .babel_status
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_logs() -> Result<()> {
        let mut test_env = setup_test_env()?;

        test_env
            .logs_tx
            .send("log1".to_string())
            .expect("failed to send log");
        test_env
            .logs_tx
            .send("log2".to_string())
            .expect("failed to send log");
        test_env
            .logs_tx
            .send("log3".to_string())
            .expect("failed to send log");

        let mut stream = test_env
            .client
            .get_logs(pb::GetLogsRequest {})
            .await?
            .into_inner();

        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = stream.next().await {
            logs.push(log.log);
        }

        assert_eq!(vec!["log1", "log2", "log3"], logs);
        Ok(())
    }
}
