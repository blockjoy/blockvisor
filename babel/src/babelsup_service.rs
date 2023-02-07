use crate::{supervisor, utils};
use async_trait::async_trait;
use babel_api::config::SupervisorConfig;
use futures::StreamExt;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::Arc;
use std::{fs, mem};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{broadcast, Mutex};
use tonic::{Request, Response, Status, Streaming};

pub enum SupervisorSetup {
    LogsRx(broadcast::Receiver<String>),
    SetupTx(supervisor::SupervisorSetupTx),
}

/// Trait that allows to inject custom actions performed on Supervisor config setup.
#[async_trait]
pub trait SupervisorConfigObserver {
    async fn supervisor_config_set(&self, cfg: &SupervisorConfig) -> eyre::Result<()>;
}

pub struct BabelSupService<T: SupervisorConfigObserver> {
    sup_setup: Arc<Mutex<SupervisorSetup>>,
    babel_change_tx: supervisor::BabelChangeTx,
    babel_bin_path: PathBuf,
    supervisor_cfg_path: PathBuf,
    supervisor_cfg_observer: T,
}

#[tonic::async_trait]
impl<T: SupervisorConfigObserver + Sync + Send + 'static> babel_api::babel_sup_server::BabelSup
    for BabelSupService<T>
{
    async fn get_version(&self, _request: Request<()>) -> Result<Response<String>, Status> {
        Ok(Response::new(env!("CARGO_PKG_VERSION").to_string()))
    }

    type GetLogsStream = tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>;

    async fn get_logs(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::GetLogsStream>, Status> {
        let mut logs = Vec::default();
        if let SupervisorSetup::LogsRx(rx) = self.sup_setup.lock().await.deref_mut() {
            loop {
                match rx.try_recv() {
                    Ok(log) => logs.push(Ok(log)),
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
        request: Request<u32>,
    ) -> Result<Response<babel_api::BabelStatus>, Status> {
        let expected_checksum = request.into_inner();
        let babel_status = match *self.babel_change_tx.borrow() {
            Some(checksum) => {
                if checksum == expected_checksum {
                    babel_api::BabelStatus::Ok
                } else {
                    babel_api::BabelStatus::ChecksumMismatch
                }
            }
            None => babel_api::BabelStatus::Missing,
        };
        Ok(Response::new(babel_status))
    }

    async fn start_new_babel(
        &self,
        request: Request<Streaming<babel_api::BabelBin>>,
    ) -> Result<Response<()>, Status> {
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
            Ok(Response::new(()))
        } else {
            Err(Status::internal(format!(
                "received babel binary checksum ({checksum})\
                 doesn't match expected ({expected_checksum})"
            )))
        }
    }

    async fn setup_supervisor(
        &self,
        request: Request<SupervisorConfig>,
    ) -> Result<Response<()>, Status> {
        let mut sup_setup = self.sup_setup.lock().await;
        if let SupervisorSetup::SetupTx(_) = sup_setup.deref() {
            let cfg = request.into_inner();
            let cfg_str = serde_json::to_string(&cfg).map_err(|err| {
                Status::internal(format!("failed to serialize supervisor config: {err}"))
            })?;
            let _ = fs::remove_file(&self.supervisor_cfg_path);
            fs::write(&self.supervisor_cfg_path, &cfg_str).map_err(|err| {
                Status::internal(format!(
                    "failed to save supervisor config into {}: {}",
                    &self.supervisor_cfg_path.to_string_lossy(),
                    err
                ))
            })?;
            self.supervisor_cfg_observer
                .supervisor_config_set(&cfg)
                .await
                .map_err(|err| Status::internal(format!("{err}")))?;
            let setup = supervisor::SupervisorSetup::new(cfg);
            let logs_tx = SupervisorSetup::LogsRx(setup.log_buffer.subscribe());
            if let SupervisorSetup::SetupTx(sup_setup_tx) =
                mem::replace(sup_setup.deref_mut(), logs_tx)
            {
                sup_setup_tx
                    .send(setup)
                    .map_err(|_| Status::internal("failed to setup supervisor"))?;
            } else {
                panic!()
            }
        }
        Ok(Response::new(()))
    }
}

impl<T: SupervisorConfigObserver> BabelSupService<T> {
    pub fn new(
        sup_setup: SupervisorSetup,
        babel_change_tx: supervisor::BabelChangeTx,
        babel_bin_path: PathBuf,
        supervisor_cfg_path: PathBuf,
        supervisor_cfg_observer: T,
    ) -> Self {
        Self {
            sup_setup: Arc::new(Mutex::new(sup_setup)),
            babel_change_tx,
            babel_bin_path,
            supervisor_cfg_path,
            supervisor_cfg_observer,
        }
    }

    /// Write babel binary stream into the file.
    async fn save_babel_stream(
        &self,
        stream: &mut Streaming<babel_api::BabelBin>,
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
            match part {
                babel_api::BabelBin::Bin(bin) => {
                    writer.write(&bin).await.map_err(|err| {
                        Status::internal(format!("failed to save babel binary: {err}"))
                    })?;
                }
                babel_api::BabelBin::Checksum(checksum) => {
                    expected_checksum = Some(checksum);
                }
            }
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
    use crate::supervisor::BabelChangeRx;
    use assert_fs::TempDir;
    use babel_api::babel_sup_client::BabelSupClient;
    use babel_api::babel_sup_server::BabelSup;
    use babel_api::config::{Entrypoint, SupervisorConfig};
    use eyre::Result;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::{oneshot, watch};
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::{Channel, Endpoint, Server, Uri};

    struct DummyObserver;

    #[async_trait]
    impl SupervisorConfigObserver for DummyObserver {
        async fn supervisor_config_set(&self, _cfg: &SupervisorConfig) -> Result<()> {
            Ok(())
        }
    }

    async fn sup_server(
        babel_path: PathBuf,
        babelsup_cfg_path: PathBuf,
        sup_setup: SupervisorSetup,
        babel_change_tx: supervisor::BabelChangeTx,
        uds_stream: UnixListenerStream,
    ) -> Result<()> {
        let sup_service = BabelSupService::new(
            sup_setup,
            babel_change_tx,
            babel_path,
            babelsup_cfg_path,
            DummyObserver {},
        );
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(babel_api::babel_sup_server::BabelSupServer::new(
                sup_service,
            ))
            .serve_with_incoming(uds_stream)
            .await?;
        Ok(())
    }

    fn test_client(tmp_root: &Path) -> Result<BabelSupClient<Channel>> {
        let socket_path = tmp_root.join("test_socket");
        let channel = Endpoint::from_static("http://[::]:50052")
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
                SupervisorSetup::SetupTx(sup_setup_tx),
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
            babel_api::BabelBin::Bin(vec![1, 2, 3, 4, 6, 7, 8, 9, 10]),
            babel_api::BabelBin::Bin(vec![11, 12, 13, 14, 16, 17, 18, 19, 20]),
            babel_api::BabelBin::Bin(vec![21, 22, 23, 24, 26, 27, 28, 29, 30]),
        ];

        test_env
            .client
            .start_new_babel(tokio_stream::iter(incomplete_babel_bin.clone()))
            .await
            .unwrap_err();
        assert!(!test_env.babel_change_rx.has_changed()?);

        let mut invalid_babel_bin = incomplete_babel_bin.clone();
        invalid_babel_bin.push(babel_api::BabelBin::Checksum(123));
        test_env
            .client
            .start_new_babel(tokio_stream::iter(invalid_babel_bin))
            .await
            .unwrap_err();
        assert!(!test_env.babel_change_rx.has_changed()?);

        let mut babel_bin = incomplete_babel_bin.clone();
        babel_bin.push(babel_api::BabelBin::Checksum(4135829304));
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
            SupervisorSetup::SetupTx(sup_setup_tx),
            babel_change_tx,
            babel_bin_path.clone(),
            Default::default(),
            DummyObserver {},
        );

        assert_eq!(
            babel_api::BabelStatus::Missing,
            sup_service
                .check_babel(Request::new(123))
                .await?
                .into_inner()
        );

        let (babel_change_tx, _) = watch::channel(Some(321));
        let (sup_setup_tx, _) = oneshot::channel();
        let sup_service = BabelSupService::new(
            SupervisorSetup::SetupTx(sup_setup_tx),
            babel_change_tx,
            babel_bin_path.clone(),
            Default::default(),
            DummyObserver {},
        );

        assert_eq!(
            babel_api::BabelStatus::ChecksumMismatch,
            sup_service
                .check_babel(Request::new(123))
                .await?
                .into_inner()
        );
        assert_eq!(
            babel_api::BabelStatus::Ok,
            sup_service
                .check_babel(Request::new(321))
                .await?
                .into_inner()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_logs() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let mut stream = test_env.client.get_logs(()).await?.into_inner();

        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = stream.next().await {
            logs.push(log);
        }

        assert_eq!(Vec::<String>::default(), logs);

        test_env
            .client
            .setup_supervisor(SupervisorConfig {
                log_buffer_capacity_ln: 10,
                entry_point: vec![Entrypoint {
                    name: "echo".to_owned(),
                    body: "echo".to_owned(),
                }],

                ..Default::default()
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

        let mut stream = test_env.client.get_logs(()).await?.into_inner();

        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = stream.next().await {
            logs.push(log);
        }

        assert_eq!(vec!["log1", "log2", "log3"], logs);
        Ok(())
    }

    #[tokio::test]
    async fn test_setup_supervisor() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let config = SupervisorConfig {
            log_buffer_capacity_ln: 10,
            entry_point: vec![Entrypoint {
                name: "echo".to_owned(),
                body: "echo".to_owned(),
            }],

            ..Default::default()
        };
        test_env.client.setup_supervisor(config.clone()).await?;
        assert_eq!(
            config,
            serde_json::from_str(&fs::read_to_string(test_env.babelsup_cfg_path)?)?
        );
        assert_eq!(config, test_env.sup_setup_rx.try_recv()?.config);
        Ok(())
    }
}
