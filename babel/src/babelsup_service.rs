use crate::babelsup_service::pb::{
    start_new_babel_request, SetupSupervisorRequest, SetupSupervisorResponse, StartNewBabelRequest,
};
use crate::{supervisor, utils};
use async_trait::async_trait;
use babel_api::config::SupervisorConfig;
use futures::StreamExt;
use std::fs;
use std::ops::DerefMut;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{broadcast, Mutex};
use tonic::{Request, Response, Status, Streaming};

pub mod pb {
    #![allow(clippy::derive_partial_eq_without_eq)]
    tonic::include_proto!("blockjoy.babelsup.v1");
}

pub struct BabelSupService {
    logs_rx: Arc<Mutex<Option<broadcast::Receiver<String>>>>,
    sup_setup_tx: Arc<Mutex<supervisor::SupervisorSetupTx>>,
    babel_change_tx: supervisor::BabelChangeTx,
    babel_bin_path: PathBuf,
    supervisor_cfg_path: PathBuf,
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
        if let Some(rx) = self.logs_rx.lock().await.deref_mut() {
            loop {
                match rx.try_recv() {
                    Ok(log) => logs.push(Ok(pb::GetLogsResponse { log })),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
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

    async fn setup_supervisor(
        &self,
        request: Request<SetupSupervisorRequest>,
    ) -> Result<Response<SetupSupervisorResponse>, Status> {
        let mut sup_setup_tx = self.sup_setup_tx.lock().await;
        if sup_setup_tx.is_some() {
            let cfg_str = request.into_inner().config;
            let cfg: SupervisorConfig = supervisor::load_config(&cfg_str).map_err(|err| {
                Status::invalid_argument(format!("invalid supervisor config: {err}"))
            })?;
            let _ = fs::remove_file(&self.supervisor_cfg_path);
            fs::write(&self.supervisor_cfg_path, &cfg_str).map_err(|err| {
                Status::internal(format!(
                    "failed to save supervisor config into {}: {}",
                    &self.supervisor_cfg_path.to_string_lossy(),
                    err
                ))
            })?;
            let setup = supervisor::SupervisorSetup::new(cfg);
            self.logs_rx
                .lock()
                .await
                .deref_mut()
                .replace(setup.log_buffer.subscribe());

            sup_setup_tx
                .take()
                .unwrap()
                .send(setup)
                .map_err(|_| Status::internal("failed to setup supervisor"))?;
        }
        Ok(Response::new(pb::SetupSupervisorResponse {}))
    }
}

impl BabelSupService {
    pub fn new(
        sup_setup_tx: supervisor::SupervisorSetupTx,
        babel_change_tx: supervisor::BabelChangeTx,
        babel_bin_path: PathBuf,
        supervisor_cfg_path: PathBuf,
    ) -> Self {
        Self {
            logs_rx: Arc::new(Mutex::new(None)),
            sup_setup_tx: Arc::new(Mutex::new(sup_setup_tx)),
            babel_change_tx,
            babel_bin_path,
            supervisor_cfg_path,
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
    use crate::supervisor::{BabelChangeRx, SupervisorSetupTx};
    use assert_fs::TempDir;
    use babel_api::config::{Entrypoint, SupervisorConfig};
    use eyre::Result;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::{oneshot, watch};
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::{Channel, Endpoint, Server, Uri};

    async fn sup_server(
        babel_path: PathBuf,
        babelsup_cfg_path: PathBuf,
        sup_setup_tx: SupervisorSetupTx,
        babel_change_tx: supervisor::BabelChangeTx,
        uds_stream: UnixListenerStream,
    ) -> Result<()> {
        let sup_service =
            BabelSupService::new(sup_setup_tx, babel_change_tx, babel_path, babelsup_cfg_path);
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
        babelsup_cfg_path: PathBuf,
        sup_setup_rx: supervisor::SupervisorSetupRx,
        babel_change_rx: BabelChangeRx,
        client: BabelSupClient<Channel>,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_root)?;
        let babel_path = tmp_root.join("babel");
        let babelsup_cfg_path = tmp_root.join("babelsup.conf");
        let client = test_client(&tmp_root)?;
        let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(
            tmp_root.join("test_socket"),
        )?);
        let (babel_change_tx, babel_change_rx) = watch::channel(None);
        let (sup_setup_tx, sup_setup_rx) = oneshot::channel();
        let babel_bin_path = babel_path.clone();
        let babelsup_config_path = babelsup_cfg_path.clone();
        tokio::spawn(async move {
            sup_server(
                babel_bin_path,
                babelsup_config_path,
                Some(sup_setup_tx),
                babel_change_tx,
                uds_stream,
            )
            .await
        });

        Ok(TestEnv {
            babel_path,
            babelsup_cfg_path,
            sup_setup_rx,
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
        let (sup_setup_tx, _) = oneshot::channel();

        let (babel_change_tx, _) = watch::channel(None);
        let sup_service = BabelSupService::new(
            Some(sup_setup_tx),
            babel_change_tx,
            babel_bin_path.clone(),
            Default::default(),
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
        let (sup_setup_tx, _) = oneshot::channel();
        let sup_service = BabelSupService::new(
            Some(sup_setup_tx),
            babel_change_tx,
            babel_bin_path.clone(),
            Default::default(),
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

        let mut stream = test_env
            .client
            .get_logs(pb::GetLogsRequest {})
            .await?
            .into_inner();

        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = stream.next().await {
            logs.push(log.log);
        }

        assert_eq!(Vec::<String>::default(), logs);

        test_env
            .client
            .setup_supervisor(pb::SetupSupervisorRequest {
                config: toml::to_string(&SupervisorConfig {
                    log_buffer_capacity_ln: 10,
                    entry_point: vec![Entrypoint {
                        command: "echo".to_owned(),
                        args: vec![],
                    }],

                    ..Default::default()
                })?,
            })
            .await?;
        let logs_tx = test_env.sup_setup_rx.await?.log_buffer;
        logs_tx
            .send("log1".to_string())
            .expect("failed to send log");
        logs_tx
            .send("log2".to_string())
            .expect("failed to send log");
        logs_tx
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

    #[tokio::test]
    async fn test_setup_supervisor() -> Result<()> {
        let mut test_env = setup_test_env()?;

        assert!(test_env
            .client
            .setup_supervisor(pb::SetupSupervisorRequest {
                config: "invalid config".to_string(),
            })
            .await
            .is_err());
        assert!(test_env.sup_setup_rx.try_recv().is_err());
        let config = toml::to_string(&SupervisorConfig {
            log_buffer_capacity_ln: 10,
            entry_point: vec![Entrypoint {
                command: "echo".to_owned(),
                args: vec![],
            }],

            ..Default::default()
        })?;
        test_env
            .client
            .setup_supervisor(pb::SetupSupervisorRequest {
                config: config.clone(),
            })
            .await?;
        assert_eq!(config, fs::read_to_string(test_env.babelsup_cfg_path)?);
        assert_eq!(
            config,
            toml::to_string(&test_env.sup_setup_rx.try_recv()?.config)?
        );
        Ok(())
    }
}
