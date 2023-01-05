use crate::config::Config;
use crate::services::api::with_auth;
use crate::services::cookbook::{
    cb_pb, cb_pb::bundle_service_client::BundleServiceClient, cb_pb::BundleIdentifier,
};
use crate::{installer, utils};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::cmp::Ordering;
use std::env;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::time::sleep;
use tonic::transport::Channel;

const BUNDLES_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BUNDLES_REQ_TIMEOUT: Duration = Duration::from_secs(5);
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct SysTimer;
#[async_trait]
impl Sleeper for SysTimer {
    async fn sleep(duration: Duration) {
        sleep(duration).await
    }
}

#[async_trait]
pub trait Sleeper {
    async fn sleep(duration: Duration);
}

pub struct SelfUpdater<T: Sleeper> {
    blacklist_path: PathBuf,
    download_path: PathBuf,
    check_interval: Option<Duration>,
    auth_token: String,
    bundles: BundleServiceClient<Channel>,
    phantom: PhantomData<T>,
}

impl<T: Sleeper> SelfUpdater<T> {
    pub fn new(cfg: &Config) -> Result<Self> {
        let download_path = crate::env::VARS_DIR.join("downloads");
        std::fs::create_dir_all(&download_path)?;
        Ok(Self {
            blacklist_path: crate::env::ROOT_DIR
                .join(installer::INSTALL_PATH)
                .join(installer::BLACKLIST),
            download_path,
            check_interval: cfg.update_check_interval_secs.map(Duration::from_secs),
            auth_token: cfg.token.to_string(),
            bundles: BundleServiceClient::new(
                Channel::from_shared(cfg.blockjoy_registry_url.clone())?
                    .timeout(BUNDLES_REQ_TIMEOUT)
                    .connect_timeout(BUNDLES_CONNECT_TIMEOUT)
                    .connect_lazy(),
            ),
            phantom: Default::default(),
        })
    }

    pub async fn run(mut self) {
        if let Some(check_interval) = self.check_interval {
            while self.check_for_update().await {
                T::sleep(check_interval).await;
            }
        }
    }

    async fn check_for_update(&mut self) -> bool {
        if let Some(latest_bundle) = self.get_latest().await {
            if let Ordering::Greater = utils::semver_cmp(&latest_bundle.version, CURRENT_VERSION) {
                if !self
                    .is_blacklisted(&latest_bundle.version)
                    .await
                    .unwrap_or(true)
                    && self.download_and_install(latest_bundle).await.is_ok()
                {
                    // stop update checker
                    return false;
                }
            }
        }
        true
    }

    pub async fn get_latest(&mut self) -> Option<cb_pb::BundleIdentifier> {
        let mut resp = self
            .bundles
            .list_bundle_versions(with_auth(
                cb_pb::BundleVersionsRequest {
                    status: cb_pb::StatusName::Development.into(),
                },
                &self.auth_token,
            ))
            .await
            .ok()?
            .into_inner();
        resp.identifiers
            .sort_by(|a, b| utils::semver_cmp(&b.version, &a.version));
        resp.identifiers.first().cloned()
    }

    async fn is_blacklisted(&self, version: &str) -> Result<bool> {
        Ok(self.blacklist_path.exists()
            && fs::read_to_string(&self.blacklist_path)
                .await
                .with_context(|| "failed to read blacklist")?
                .contains(version))
    }

    pub async fn download_and_install(&mut self, bundle: BundleIdentifier) -> Result<()> {
        let archive = self
            .bundles
            .retrieve(with_auth(bundle, &self.auth_token))
            .await?
            .into_inner();

        let bundle_path = self.download_path.join("bundle");
        let _ = fs::remove_dir_all(&bundle_path).await;

        utils::download_archive(
            &archive.url.clone(),
            self.download_path.join("bundle.tar.gz"),
        )
        .await
        .with_context(|| "failed to download bundle")?
        .ungzip()
        .await
        .with_context(|| "failed to extract downloaded bundle")?
        .untar()
        .await
        .with_context(|| "failed to extract downloaded bundle")?;

        Command::new(bundle_path.join(installer::INSTALLER_BIN)).spawn()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::tests::test_channel;
    use assert_fs::TempDir;
    use httpmock::prelude::GET;
    use httpmock::MockServer;
    use mockall::*;
    use std::ffi::OsStr;
    use tokio::io::AsyncWriteExt;
    use tonic::Response;

    mock! {
        pub TestBundleService {}

        #[tonic::async_trait]
        impl cb_pb::bundle_service_server::BundleService for TestBundleService {
            /// Retrieve image for specific version and state
            async fn retrieve(
                &self,
                request: tonic::Request<cb_pb::BundleIdentifier>,
            ) -> Result<tonic::Response<cb_pb::ArchiveLocation>, tonic::Status>;
            /// List all available bundle versions
            async fn list_bundle_versions(
                &self,
                request: tonic::Request<cb_pb::BundleVersionsRequest>,
            ) -> Result<tonic::Response<cb_pb::BundleVersionsResponse>, tonic::Status>;
            /// Promote/Demote bundle to the desired state
            async fn add_stage(
                &self,
                request: tonic::Request<cb_pb::BundleIdentifier>,
            ) -> Result<tonic::Response<()>, tonic::Status>;
            /// Disable configuration in the desired state
            async fn remove_stage(
                &self,
                request: tonic::Request<cb_pb::BundleIdentifier>,
            ) -> Result<tonic::Response<()>, tonic::Status>;
            /// Delete bundle from storage
            async fn delete(
                &self,
                request: tonic::Request<cb_pb::BundleIdentifier>,
            ) -> Result<tonic::Response<()>, tonic::Status>;
        }
    }

    mock! {
        pub TestSleeper {}

        #[async_trait]
        impl Sleeper for TestSleeper {
            async fn sleep(duration: Duration);
        }
    }

    /// Common staff to setup for all tests like sut (self updater in that case),
    /// path to root dir used in test, instance of AsyncPanicChecker to make sure that all panics
    /// from other threads will be propagated.
    struct TestEnv {
        updater: SelfUpdater<MockTestSleeper>,
        blacklist_path: PathBuf,
        tmp_root: PathBuf,
        _async_panic_checker: utils::tests::AsyncPanicChecker,
    }

    impl TestEnv {
        async fn new() -> Result<Self> {
            let tmp_root = TempDir::new()?.to_path_buf();
            let download_path = tmp_root.join("downloads");
            fs::create_dir_all(&download_path).await?;
            let blacklist_path = tmp_root.join(installer::BLACKLIST);

            Ok(Self {
                updater: SelfUpdater::<MockTestSleeper> {
                    blacklist_path: blacklist_path.clone(),
                    download_path,
                    check_interval: Some(Duration::from_secs(3)),
                    auth_token: "test_token".to_string(),
                    bundles: BundleServiceClient::new(test_channel(&tmp_root)),
                    phantom: Default::default(),
                },
                blacklist_path,
                tmp_root,
                _async_panic_checker: Default::default(),
            })
        }

        fn start_test_server(
            &self,
            bundles_mock: MockTestBundleService,
        ) -> utils::tests::TestServer {
            utils::tests::start_test_server(
                self.tmp_root.join("test_socket"),
                cb_pb::bundle_service_server::BundleServiceServer::new(bundles_mock),
            )
        }

        async fn blacklist_version(&self, version: &str) -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .append(true)
                .create(true)
                .open(&self.blacklist_path)
                .await?;
            file.write_all(format!("{version}").as_bytes()).await?;
            Ok(())
        }

        async fn create_dummy_bundle(&self, ctrl_file_path: &str) -> Result<()> {
            let bundle_path = self.tmp_root.join("bundle");
            fs::create_dir_all(&bundle_path).await?;
            // create dummy installer that will touch control file
            let mut installer = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o770)
                .open(bundle_path.join(installer::INSTALLER_BIN))
                .await?;
            installer
                .write_all(format!("#!/bin/sh\ntouch {ctrl_file_path}\n").as_bytes())
                .await?;

            utils::run_cmd(
                "tar",
                [
                    OsStr::new("-C"),
                    self.tmp_root.as_os_str(),
                    OsStr::new("-czf"),
                    self.tmp_root.join("bundle.tar.gz").as_os_str(),
                    OsStr::new("bundle"),
                ],
            )
            .await?;

            Ok(())
        }
    }

    async fn wait_for_ctrl_file(ctrl_file_path: &PathBuf) {
        tokio::time::timeout(Duration::from_millis(500), async {
            while !ctrl_file_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .ok();
    }

    #[tokio::test]
    async fn test_get_latest() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let bundle_id = BundleIdentifier {
            version: "3.2.1".to_string(),
        };

        // no server
        assert_eq!(None, test_env.updater.get_latest().await);

        let mut bundles_mock = MockTestBundleService::new();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(|_| {
                let reply = cb_pb::BundleVersionsResponse {
                    identifiers: vec![],
                };
                Ok(Response::new(reply))
            });
        let expected_bundle_id = bundle_id.clone();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(move |_| {
                let reply = cb_pb::BundleVersionsResponse {
                    identifiers: vec![
                        cb_pb::BundleIdentifier {
                            version: "1.2.3".to_string(),
                        },
                        expected_bundle_id.clone(),
                        cb_pb::BundleIdentifier {
                            version: "0.1.2".to_string(),
                        },
                    ],
                };
                Ok(Response::new(reply))
            });
        let bundle_server = test_env.start_test_server(bundles_mock);

        assert_eq!(None, test_env.updater.get_latest().await);
        assert_eq!(Some(bundle_id), test_env.updater.get_latest().await);
        bundle_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_download_failed() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let bundle_id = BundleIdentifier {
            version: "3.2.1".to_string(),
        };
        let server = MockServer::start();

        // no server
        assert!(test_env
            .updater
            .download_and_install(bundle_id.clone())
            .await
            .is_err());

        let mut bundles_mock = MockTestBundleService::new();
        bundles_mock.expect_retrieve().once().returning(|_| {
            let reply = cb_pb::ArchiveLocation {
                url: "invalid_url".to_string(),
            };
            Ok(Response::new(reply))
        });
        let server_addres = server.address().to_string();
        bundles_mock.expect_retrieve().once().returning(move |_| {
            let reply = cb_pb::ArchiveLocation {
                url: format!("http://{server_addres}"),
            };
            Ok(Response::new(reply))
        });
        let bundle_server = test_env.start_test_server(bundles_mock);

        assert!(test_env
            .updater
            .download_and_install(bundle_id.clone())
            .await
            .is_err());

        let mock = server.mock(|when, then| {
            when.method(GET);
            then.status(200).body("invalid");
        });

        assert!(test_env
            .updater
            .download_and_install(bundle_id.clone())
            .await
            .is_err());
        mock.assert();
        bundle_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_download_and_install() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let ctrl_file_path = test_env.tmp_root.join("ctrl_file");
        let bundle_id = BundleIdentifier {
            version: "3.2.1".to_string(),
        };
        let server = MockServer::start();

        let mut bundles_mock = MockTestBundleService::new();
        let server_addres = server.address().to_string();
        bundles_mock.expect_retrieve().once().returning(move |_| {
            let reply = cb_pb::ArchiveLocation {
                url: format!("http://{server_addres}"),
            };
            Ok(Response::new(reply))
        });
        let bundle_server = test_env.start_test_server(bundles_mock);

        test_env
            .create_dummy_bundle(&ctrl_file_path.to_string_lossy())
            .await?;
        fs::remove_file(&ctrl_file_path).await.ok();

        let mock = server.mock(|when, then| {
            when.method(GET);
            then.status(200)
                .body_from_file(&*test_env.tmp_root.join("bundle.tar.gz").to_string_lossy());
        });

        test_env
            .updater
            .download_and_install(bundle_id.clone())
            .await?;

        wait_for_ctrl_file(&ctrl_file_path).await;
        assert!(ctrl_file_path.exists());

        mock.assert();
        bundle_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_check_for_update() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let ctrl_file_path = test_env.tmp_root.join("ctrl_file");
        let bundle_id = BundleIdentifier {
            version: "3.2.1".to_string(),
        };

        // continue if no update installed
        assert!(test_env.updater.check_for_update().await);

        let server = MockServer::start();

        let mut bundles_mock = MockTestBundleService::new();
        let expected_bundle_id = bundle_id.clone();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(move |_| {
                let reply = cb_pb::BundleVersionsResponse {
                    identifiers: vec![expected_bundle_id.clone()],
                };
                Ok(Response::new(reply))
            });
        let server_addres = server.address().to_string();
        bundles_mock
            .expect_retrieve()
            .once()
            .withf(move |req| req.get_ref() == &bundle_id)
            .returning(move |_| {
                let reply = cb_pb::ArchiveLocation {
                    url: format!("http://{server_addres}"),
                };
                Ok(Response::new(reply))
            });
        let bundle_server = test_env.start_test_server(bundles_mock);

        test_env
            .create_dummy_bundle(&ctrl_file_path.to_string_lossy())
            .await?;

        fs::remove_file(&ctrl_file_path).await.ok();

        let mock = server.mock(|when, then| {
            when.method(GET);
            then.status(200)
                .body_from_file(&*test_env.tmp_root.join("bundle.tar.gz").to_string_lossy());
        });

        assert!(!test_env.updater.check_for_update().await);

        wait_for_ctrl_file(&ctrl_file_path).await;
        assert!(ctrl_file_path.exists());

        mock.assert();
        bundle_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_check_for_update_blacklisted() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let bundle_id = BundleIdentifier {
            version: "3.2.1".to_string(),
        };
        test_env.blacklist_version(&bundle_id.version).await?;

        let mut bundles_mock = MockTestBundleService::new();
        let expected_bundle_id = bundle_id.clone();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(move |_| {
                let reply = cb_pb::BundleVersionsResponse {
                    identifiers: vec![expected_bundle_id.clone()],
                };
                Ok(Response::new(reply))
            });
        let bundle_server = test_env.start_test_server(bundles_mock);

        assert!(test_env.updater.check_for_update().await);
        bundle_server.assert().await;
        Ok(())
    }
}
