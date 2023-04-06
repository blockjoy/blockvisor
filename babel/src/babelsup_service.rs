use crate::{supervisor, utils};
use async_trait::async_trait;
use babel_api::config::SupervisorConfig;
use std::{
    fs, mem,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status, Streaming};

pub enum SupervisorStatus {
    Ready,
    Uninitialized(supervisor::SupervisorConfigTx),
}

/// Trait that allows to inject custom actions performed on Supervisor config setup.
#[async_trait]
pub trait SupervisorConfigObserver {
    async fn supervisor_config_set(&self, cfg: &SupervisorConfig) -> eyre::Result<()>;
}

pub struct BabelSupService {
    status: Arc<Mutex<SupervisorStatus>>,
    babel_change_tx: supervisor::BabelChangeTx,
    babel_bin_path: PathBuf,
    supervisor_cfg_path: PathBuf,
}

#[tonic::async_trait]
impl babel_api::babel_sup_server::BabelSup for BabelSupService {
    async fn get_version(&self, _request: Request<()>) -> Result<Response<String>, Status> {
        Ok(Response::new(env!("CARGO_PKG_VERSION").to_string()))
    }

    async fn check_babel(
        &self,
        request: Request<u32>,
    ) -> Result<Response<babel_api::BinaryStatus>, Status> {
        let expected_checksum = request.into_inner();
        let babel_status = match *self.babel_change_tx.borrow() {
            Some(checksum) => {
                if checksum == expected_checksum {
                    babel_api::BinaryStatus::Ok
                } else {
                    babel_api::BinaryStatus::ChecksumMismatch
                }
            }
            None => babel_api::BinaryStatus::Missing,
        };
        Ok(Response::new(babel_status))
    }

    async fn start_new_babel(
        &self,
        request: Request<Streaming<babel_api::Binary>>,
    ) -> Result<Response<()>, Status> {
        let mut stream = request.into_inner();
        let checksum = utils::save_bin_stream(&self.babel_bin_path, &mut stream)
            .await
            .map_err(|e| Status::internal(format!("start_new_babel failed with {e}")))?;
        self.babel_change_tx.send_modify(|value| {
            let _ = value.insert(checksum);
        });
        Ok(Response::new(()))
    }

    async fn setup_supervisor(
        &self,
        request: Request<SupervisorConfig>,
    ) -> Result<Response<()>, Status> {
        let mut status = self.status.lock().await;
        if let SupervisorStatus::Uninitialized(_) = status.deref() {
            let cfg = request.into_inner();
            let cfg_str = serde_json::to_string(&cfg).map_err(|err| {
                Status::internal(format!("failed to serialize supervisor config: {err}"))
            })?;
            let _ = fs::remove_file(&self.supervisor_cfg_path);
            fs::write(&self.supervisor_cfg_path, cfg_str).map_err(|err| {
                Status::internal(format!(
                    "failed to save supervisor config into {}: {}",
                    &self.supervisor_cfg_path.to_string_lossy(),
                    err
                ))
            })?;
            if let SupervisorStatus::Uninitialized(sup_config_tx) =
                mem::replace(status.deref_mut(), SupervisorStatus::Ready)
            {
                sup_config_tx
                    .send(cfg)
                    .map_err(|_| Status::internal("failed to setup supervisor"))?;
            } else {
                unreachable!()
            }
        }
        Ok(Response::new(()))
    }
}

impl BabelSupService {
    pub fn new(
        status: SupervisorStatus,
        babel_change_tx: supervisor::BabelChangeTx,
        babel_bin_path: PathBuf,
        supervisor_cfg_path: PathBuf,
    ) -> Self {
        Self {
            status: Arc::new(Mutex::new(status)),
            babel_change_tx,
            babel_bin_path,
            supervisor_cfg_path,
        }
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

    async fn sup_server(
        babel_path: PathBuf,
        babelsup_cfg_path: PathBuf,
        sup_status: SupervisorStatus,
        babel_change_tx: supervisor::BabelChangeTx,
        uds_stream: UnixListenerStream,
    ) -> Result<()> {
        let sup_service =
            BabelSupService::new(sup_status, babel_change_tx, babel_path, babelsup_cfg_path);
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(babel_api::babel_sup_server::BabelSupServer::new(
                sup_service,
            ))
            .serve_with_incoming(uds_stream)
            .await?;
        Ok(())
    }

    fn test_client(tmp_root: &Path) -> BabelSupClient<Channel> {
        let socket_path = tmp_root.join("test_socket");
        let channel = Endpoint::from_static("http://[::]:50052")
            .timeout(Duration::from_secs(1))
            .connect_timeout(Duration::from_secs(1))
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                UnixStream::connect(socket_path.clone())
            }));
        BabelSupClient::new(channel)
    }

    struct TestEnv {
        babel_path: PathBuf,
        babelsup_cfg_path: PathBuf,
        sup_config_rx: supervisor::SupervisorConfigRx,
        babel_change_rx: BabelChangeRx,
        client: BabelSupClient<Channel>,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        fs::create_dir_all(&tmp_root)?;
        let babel_path = tmp_root.join("babel");
        let babelsup_cfg_path = tmp_root.join("babelsup.conf");
        let client = test_client(&tmp_root);
        let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(
            tmp_root.join("test_socket"),
        )?);
        let (babel_change_tx, babel_change_rx) = watch::channel(None);
        let (sup_config_tx, sup_config_rx) = oneshot::channel();
        let babel_bin_path = babel_path.clone();
        let babelsup_config_path = babelsup_cfg_path.clone();
        tokio::spawn(async move {
            sup_server(
                babel_bin_path,
                babelsup_config_path,
                SupervisorStatus::Uninitialized(sup_config_tx),
                babel_change_tx,
                uds_stream,
            )
            .await
        });

        Ok(TestEnv {
            babel_path,
            babelsup_cfg_path,
            sup_config_rx,
            babel_change_rx,
            client,
        })
    }

    #[tokio::test]
    async fn test_start_new_babel() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let incomplete_babel_bin = vec![
            babel_api::Binary::Bin(vec![1, 2, 3, 4, 6, 7, 8, 9, 10]),
            babel_api::Binary::Bin(vec![11, 12, 13, 14, 16, 17, 18, 19, 20]),
            babel_api::Binary::Bin(vec![21, 22, 23, 24, 26, 27, 28, 29, 30]),
        ];

        test_env
            .client
            .start_new_babel(tokio_stream::iter(incomplete_babel_bin.clone()))
            .await
            .unwrap_err();
        assert!(!test_env.babel_change_rx.has_changed()?);

        let mut invalid_babel_bin = incomplete_babel_bin.clone();
        invalid_babel_bin.push(babel_api::Binary::Checksum(123));
        test_env
            .client
            .start_new_babel(tokio_stream::iter(invalid_babel_bin))
            .await
            .unwrap_err();
        assert!(!test_env.babel_change_rx.has_changed()?);

        let mut babel_bin = incomplete_babel_bin.clone();
        babel_bin.push(babel_api::Binary::Checksum(4135829304));
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
        let (sup_config_tx, _) = oneshot::channel();

        let (babel_change_tx, _) = watch::channel(None);
        let sup_service = BabelSupService::new(
            SupervisorStatus::Uninitialized(sup_config_tx),
            babel_change_tx,
            babel_bin_path.clone(),
            Default::default(),
        );

        assert_eq!(
            babel_api::BinaryStatus::Missing,
            sup_service
                .check_babel(Request::new(123))
                .await?
                .into_inner()
        );

        let (babel_change_tx, _) = watch::channel(Some(321));
        let (sup_config_tx, _) = oneshot::channel();
        let sup_service = BabelSupService::new(
            SupervisorStatus::Uninitialized(sup_config_tx),
            babel_change_tx,
            babel_bin_path.clone(),
            Default::default(),
        );

        assert_eq!(
            babel_api::BinaryStatus::ChecksumMismatch,
            sup_service
                .check_babel(Request::new(123))
                .await?
                .into_inner()
        );
        assert_eq!(
            babel_api::BinaryStatus::Ok,
            sup_service
                .check_babel(Request::new(321))
                .await?
                .into_inner()
        );
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
        assert_eq!(config, test_env.sup_config_rx.try_recv()?);
        Ok(())
    }
}
