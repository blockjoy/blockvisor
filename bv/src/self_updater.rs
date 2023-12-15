use crate::{config::SharedConfig, installer, services, services::api::pb, utils, BV_VAR_PATH};
use bv_utils::{run_flag::RunFlag, timer::AsyncTimer};
use eyre::{anyhow, Context, Result};
use std::{
    cmp::Ordering,
    env,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, process::Command};
use tracing::{debug, warn};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DOWNLOADS: &str = "downloads";
const BUNDLE: &str = "bundle";
const BUNDLE_FILE: &str = "bundle.tar.gz";

pub type DefaultConnector = services::DefaultConnector;

pub struct SelfUpdater<T, C> {
    blacklist_path: PathBuf,
    download_path: PathBuf,
    check_interval: Option<Duration>,
    bundles: C,
    latest_installed_version: String,
    sleeper: T,
}

pub async fn new<T: AsyncTimer>(
    sleeper: T,
    bv_root: &Path,
    config: &SharedConfig,
) -> Result<SelfUpdater<T, DefaultConnector>> {
    let download_path = bv_root.join(BV_VAR_PATH).join(DOWNLOADS);
    std::fs::create_dir_all(&download_path)
        .with_context(|| format!("cannot create dirs: {}", download_path.display()))?;
    Ok(SelfUpdater {
        blacklist_path: bv_root
            .join(installer::INSTALL_PATH)
            .join(installer::BLACKLIST),
        download_path,
        check_interval: config
            .read()
            .await
            .update_check_interval_secs
            .map(Duration::from_secs),
        bundles: DefaultConnector {
            config: config.clone(),
        },
        latest_installed_version: CURRENT_VERSION.to_string(),
        sleeper,
    })
}

impl<T: AsyncTimer, C: services::ApiServiceConnector> SelfUpdater<T, C> {
    pub async fn run(mut self, mut run: RunFlag) {
        if let Some(check_interval) = self.check_interval {
            while run.load() {
                if let Err(e) = self.check_for_update().await {
                    warn!("Error executing self update: {e:#}");
                }
                run.select(self.sleeper.sleep(utils::with_jitter(check_interval)))
                    .await;
            }
        }
    }

    pub async fn check_for_update(&mut self) -> Result<()> {
        if let Some(latest_bundle) = self
            .get_latest()
            .await
            .with_context(|| "cannot get latest version")?
        {
            let latest_version = latest_bundle.version.clone();
            debug!("Latest version of BV is `{latest_version}`");
            if let Ordering::Greater =
                utils::semver_cmp(&latest_version, &self.latest_installed_version)
            {
                if !self.is_blacklisted(&latest_version).await? {
                    self.download_and_install(latest_bundle).await?;
                    self.latest_installed_version = latest_version;
                }
            }
        }

        Ok(())
    }

    pub async fn get_latest(&mut self) -> Result<Option<pb::BundleIdentifier>> {
        let mut resp: pb::BundleServiceListBundleVersionsResponse = self
            .bundles
            .connect(pb::bundle_service_client::BundleServiceClient::with_interceptor)
            .await
            .with_context(|| "cannot connect to bundle service")?
            .list_bundle_versions(pb::BundleServiceListBundleVersionsRequest {})
            .await
            .with_context(|| "cannot list bundle versions")?
            .into_inner();
        resp.identifiers
            .sort_by(|a, b| utils::semver_cmp(&b.version, &a.version));
        Ok(resp.identifiers.first().cloned())
    }

    async fn is_blacklisted(&self, version: &str) -> Result<bool> {
        Ok(self.blacklist_path.exists()
            && fs::read_to_string(&self.blacklist_path)
                .await
                .with_context(|| {
                    format!(
                        "failed to read blacklist: {}",
                        self.blacklist_path.display()
                    )
                })?
                .contains(version))
    }

    pub async fn download_and_install(&mut self, bundle: pb::BundleIdentifier) -> Result<()> {
        let archive = self
            .bundles
            .connect(pb::bundle_service_client::BundleServiceClient::with_interceptor)
            .await?
            .retrieve(tonic::Request::new(pb::BundleServiceRetrieveRequest {
                id: Some(bundle),
            }))
            .await?
            .into_inner()
            .location
            .ok_or_else(|| anyhow!("missing location"))?;

        let bundle_path = self.download_path.join(BUNDLE);
        let _ = fs::remove_dir_all(&bundle_path).await;

        let url = archive.url.clone();
        utils::download_archive(&url, self.download_path.join(BUNDLE_FILE))
            .await
            .with_context(|| format!("failed to download bundle from `{url}`"))?
            .ungzip()
            .await
            .with_context(|| "failed to ungzip downloaded bundle")?
            .untar()
            .await
            .with_context(|| "failed to untar downloaded bundle")?;

        Command::new(bundle_path.join(installer::INSTALLER_BIN)).spawn()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{api::common, ApiInterceptor, AuthToken};
    use assert_fs::TempDir;
    use async_trait::async_trait;
    use bv_tests_utils::{rpc::test_channel, start_test_server};
    use bv_utils::{
        rpc::DefaultTimeout,
        {cmd::run_cmd, timer::MockAsyncTimer},
    };
    use mockall::*;
    use std::{ffi::OsStr, path::Path};
    use tokio::io::AsyncWriteExt;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::{transport::Channel, Response};

    mock! {
        pub TestBundleService {}

        #[tonic::async_trait]
        impl pb::bundle_service_server::BundleService for TestBundleService {
            /// Retrieve image for specific version and state
            async fn retrieve(
                &self,
                request: tonic::Request<pb::BundleServiceRetrieveRequest>,
            ) -> Result<tonic::Response<pb::BundleServiceRetrieveResponse>, tonic::Status>;
            /// List all available bundle versions
            async fn list_bundle_versions(
                &self,
                request: tonic::Request<pb::BundleServiceListBundleVersionsRequest>,
            ) -> Result<tonic::Response<pb::BundleServiceListBundleVersionsResponse>, tonic::Status>;
            /// Delete bundle from storage
            async fn delete(
                &self,
                request: tonic::Request<pb::BundleServiceDeleteRequest>,
            ) -> Result<tonic::Response<pb::BundleServiceDeleteResponse>, tonic::Status>;
        }
    }

    struct TestConnector {
        tmp_root: PathBuf,
    }

    #[async_trait]
    impl services::ApiServiceConnector for TestConnector {
        async fn connect<T, I>(&self, with_interceptor: I) -> Result<T>
        where
            I: Send + Sync + Fn(Channel, ApiInterceptor) -> T,
        {
            Ok(with_interceptor(
                test_channel(&self.tmp_root),
                ApiInterceptor(
                    AuthToken("test_token".to_owned()),
                    DefaultTimeout(Duration::from_secs(1)),
                ),
            ))
        }
    }

    /// Common staff to setup for all tests like sut (self updater in that case),
    /// path to root dir used in test, instance of AsyncPanicChecker to make sure that all panics
    /// from other threads will be propagated.
    struct TestEnv {
        updater: SelfUpdater<MockAsyncTimer, TestConnector>,
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
                updater: SelfUpdater {
                    blacklist_path: blacklist_path.clone(),
                    download_path,
                    check_interval: Some(Duration::from_secs(3)),
                    bundles: TestConnector {
                        tmp_root: tmp_root.clone(),
                    },
                    latest_installed_version: CURRENT_VERSION.to_string(),
                    sleeper: MockAsyncTimer::new(),
                },
                blacklist_path,
                tmp_root,
                _async_panic_checker: Default::default(),
            })
        }

        fn start_test_server(
            &self,
            bundles_mock: MockTestBundleService,
        ) -> bv_tests_utils::rpc::TestServer {
            start_test_server!(
                &self.tmp_root,
                pb::bundle_service_server::BundleServiceServer::new(bundles_mock)
            )
        }

        async fn blacklist_version(&self, version: &str) -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .append(true)
                .create(true)
                .open(&self.blacklist_path)
                .await?;
            file.write_all(version.as_bytes()).await?;
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

            run_cmd(
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

    async fn wait_for_ctrl_file(ctrl_file_path: &Path) {
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
        let bundle_id = pb::BundleIdentifier {
            version: "3.2.1".to_string(),
        };

        // no server
        let _ = test_env.updater.get_latest().await.unwrap_err();

        let mut bundles_mock = MockTestBundleService::new();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(|_| {
                let reply = pb::BundleServiceListBundleVersionsResponse {
                    identifiers: vec![],
                };
                Ok(Response::new(reply))
            });
        let expected_bundle_id = bundle_id.clone();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(move |_| {
                let reply = pb::BundleServiceListBundleVersionsResponse {
                    identifiers: vec![
                        pb::BundleIdentifier {
                            version: "1.2.3".to_string(),
                        },
                        expected_bundle_id.clone(),
                        pb::BundleIdentifier {
                            version: "0.1.2".to_string(),
                        },
                    ],
                };
                Ok(Response::new(reply))
            });
        let bundle_server = test_env.start_test_server(bundles_mock);

        assert_eq!(None, test_env.updater.get_latest().await.unwrap());
        assert_eq!(
            Some(bundle_id),
            test_env.updater.get_latest().await.unwrap()
        );
        bundle_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_download_failed() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let bundle_id = pb::BundleIdentifier {
            version: "3.2.1".to_string(),
        };
        let mut server = mockito::Server::new();

        // no server
        let _ = test_env
            .updater
            .download_and_install(bundle_id.clone())
            .await
            .unwrap_err();

        let mut bundles_mock = MockTestBundleService::new();
        bundles_mock.expect_retrieve().once().returning(|_| {
            let reply = pb::BundleServiceRetrieveResponse {
                location: Some(common::ArchiveLocation {
                    url: "invalid_url".to_string(),
                }),
            };
            Ok(Response::new(reply))
        });
        let url = server.url();
        bundles_mock.expect_retrieve().once().returning(move |_| {
            let reply = pb::BundleServiceRetrieveResponse {
                location: Some(common::ArchiveLocation { url: url.clone() }),
            };
            Ok(Response::new(reply))
        });
        let bundle_server = test_env.start_test_server(bundles_mock);

        let _ = test_env
            .updater
            .download_and_install(bundle_id.clone())
            .await
            .unwrap_err();

        let mock = server.mock("GET", "/").with_body("invalid").create();

        let _ = test_env
            .updater
            .download_and_install(bundle_id.clone())
            .await
            .unwrap_err();
        mock.assert();
        bundle_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_download_and_install() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let ctrl_file_path = test_env.tmp_root.join("ctrl_file");
        let bundle_id = pb::BundleIdentifier {
            version: "3.2.1".to_string(),
        };
        let mut server = mockito::Server::new();

        let mut bundles_mock = MockTestBundleService::new();
        let url = server.url();
        bundles_mock.expect_retrieve().once().returning(move |_| {
            let reply = pb::BundleServiceRetrieveResponse {
                location: Some(common::ArchiveLocation { url: url.clone() }),
            };
            Ok(Response::new(reply))
        });
        let bundle_server = test_env.start_test_server(bundles_mock);

        test_env
            .create_dummy_bundle(&ctrl_file_path.to_string_lossy())
            .await?;
        fs::remove_file(&ctrl_file_path).await.ok();

        let mock = server
            .mock("GET", "/")
            .with_body_from_file(&*test_env.tmp_root.join("bundle.tar.gz").to_string_lossy())
            .create();

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
        let bundle_version = "3.2.1".to_string();
        let bundle_id = pb::BundleIdentifier {
            version: bundle_version.clone(),
        };

        // continue if no update installed
        let _ = test_env.updater.check_for_update().await;
        assert_eq!(CURRENT_VERSION, test_env.updater.latest_installed_version);

        let mut server = mockito::Server::new();

        let mut bundles_mock = MockTestBundleService::new();
        let expected_bundle_id = bundle_id.clone();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(move |_| {
                let reply = pb::BundleServiceListBundleVersionsResponse {
                    identifiers: vec![expected_bundle_id.clone()],
                };
                Ok(Response::new(reply))
            });
        let url = server.url();
        bundles_mock
            .expect_retrieve()
            .once()
            .withf(
                move |req: &tonic::Request<pb::BundleServiceRetrieveRequest>| {
                    req.get_ref().id.as_ref().unwrap() == &bundle_id
                },
            )
            .returning(move |_| {
                let reply = pb::BundleServiceRetrieveResponse {
                    location: Some(common::ArchiveLocation { url: url.clone() }),
                };
                Ok(Response::new(reply))
            });
        let bundle_server = test_env.start_test_server(bundles_mock);

        test_env
            .create_dummy_bundle(&ctrl_file_path.to_string_lossy())
            .await?;

        fs::remove_file(&ctrl_file_path).await.ok();

        let mock = server
            .mock("GET", "/")
            .with_body_from_file(&*test_env.tmp_root.join("bundle.tar.gz").to_string_lossy())
            .create();

        test_env.updater.check_for_update().await?;
        assert_eq!(bundle_version, test_env.updater.latest_installed_version);

        wait_for_ctrl_file(&ctrl_file_path).await;
        assert!(ctrl_file_path.exists());

        mock.assert();
        bundle_server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_check_for_update_blacklisted() -> Result<()> {
        let mut test_env = TestEnv::new().await?;
        let bundle_id = pb::BundleIdentifier {
            version: "3.2.1".to_string(),
        };
        test_env.blacklist_version(&bundle_id.version).await?;

        let mut bundles_mock = MockTestBundleService::new();
        let expected_bundle_id = bundle_id.clone();
        bundles_mock
            .expect_list_bundle_versions()
            .once()
            .returning(move |_| {
                let reply: pb::BundleServiceListBundleVersionsResponse =
                    pb::BundleServiceListBundleVersionsResponse {
                        identifiers: vec![expected_bundle_id.clone()],
                    };
                Ok(Response::new(reply))
            });
        let bundle_server = test_env.start_test_server(bundles_mock);

        test_env.updater.check_for_update().await?;
        assert_eq!(CURRENT_VERSION, test_env.updater.latest_installed_version);
        bundle_server.assert().await;
        Ok(())
    }
}
