use crate::{
    config::{Config, CONFIG_PATH},
    linux_platform::BRIDGE_IFACE,
    server::bv_pb,
    server::bv_pb::blockvisor_client::BlockvisorClient,
    with_retry,
};
use anyhow::{anyhow, bail, ensure, Context, Error, Result};
use async_trait::async_trait;
use bv_utils::timer::Timer;
use semver::Version;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
    {env, fs},
};
use sysinfo::{System, SystemExt};
use tonic::transport::Channel;
use tracing::{debug, info, warn};

const SYSTEM_SERVICES: &str = "etc/systemd/system";
const SYSTEM_BIN: &str = "usr/bin";
pub const INSTALL_PATH: &str = "opt/blockvisor";
pub const BLACKLIST: &str = "blacklist";
const CURRENT_LINK: &str = "current";
const BACKUP_LINK: &str = "backup";
pub const INSTALLER_BIN: &str = "installer";
const FC_BIN: &str = "firecracker/bin";
const BLOCKVISOR_BIN: &str = "blockvisor/bin";
const BLOCKVISOR_SERVICES: &str = "blockvisor/services";
const BLOCKVISOR_CONFIG: &str = "blockvisor.json";
const THIS_VERSION: &str = env!("CARGO_PKG_VERSION");
const BV_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const BV_REQ_TIMEOUT: Duration = Duration::from_secs(1);
const BV_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const PREPARE_FOR_UPDATE_TIMEOUT: Duration = Duration::from_secs(180);

struct InstallerPaths {
    bv_root: PathBuf,
    system_services: PathBuf,
    system_bin: PathBuf,
    install_path: PathBuf,
    current: PathBuf,
    this_version: PathBuf,
    backup: PathBuf,
    blacklist: PathBuf,
}

/// SystemCtl abstraction for better testing.
#[async_trait]
pub trait BvService {
    async fn reload(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn start(&self) -> Result<()>;
    async fn enable(&self) -> Result<()>;
}

#[derive(Debug, PartialEq)]
enum BackupStatus {
    Done(String),
    NothingToBackup,
    ThisIsRollback,
}

pub struct Installer<T, S> {
    paths: InstallerPaths,
    bv_client: BlockvisorClient<Channel>,
    backup_status: BackupStatus,
    timer: T,
    bv_service: S,
}

impl<T: Timer, S: BvService> Installer<T, S> {
    pub async fn new(timer: T, bv_service: S, bv_root: &Path) -> Result<Self> {
        let config = Config::load(bv_root).await?;

        Ok(Self::internal_new(
            timer,
            bv_service,
            bv_root,
            Channel::from_shared(format!("http://localhost:{}", config.blockvisor_port))?
                .timeout(BV_REQ_TIMEOUT)
                .connect_timeout(BV_CONNECT_TIMEOUT)
                .connect_lazy(),
        ))
    }

    pub async fn run(mut self) -> Result<()> {
        if self.is_blacklisted(THIS_VERSION)? {
            bail!("BV {THIS_VERSION} is on a blacklist - can't install")
        }
        check_requirements().await.with_context(|| {
            format!("Host doesn't meet the requirements, see [Host Setup Guide]('https://github.com/blockjoy/bv-host-setup/releases/tag/{THIS_VERSION}') for more details.")
        })?;
        info!("installing BV {THIS_VERSION}...");

        self.preinstall()?; // TODO: try to send install failed status to the backend in error case
        if let Err(err) = self.install().await {
            self.handle_broken_installation(err).await
        } else {
            // try cleanup after install, but cleanup result should not affect exit code
            self.cleanup() // do not interrupt cleanup on errors
                .unwrap_or_else(|err| warn!("failed to cleanup after install with: {err}"));
            Ok(())
        }
    }

    fn internal_new(timer: T, bv_service: S, bv_root: &Path, bv_channel: Channel) -> Self {
        let install_path = bv_root.join(INSTALL_PATH);
        let current = install_path.join(CURRENT_LINK);
        let this_version = install_path.join(THIS_VERSION);
        let backup = install_path.join(BACKUP_LINK);
        let blacklist = install_path.join(BLACKLIST);

        Self {
            paths: InstallerPaths {
                bv_root: bv_root.to_path_buf(),
                system_services: bv_root.join(SYSTEM_SERVICES),
                system_bin: bv_root.join(SYSTEM_BIN),
                install_path,
                current,
                this_version,
                backup,
                blacklist,
            },
            bv_client: BlockvisorClient::new(bv_channel),
            backup_status: BackupStatus::NothingToBackup,
            timer,
            bv_service,
        }
    }

    fn move_bundle_to_install_path(&self, current_exe_path: PathBuf) -> Result<()> {
        let bin_path = fs::canonicalize(current_exe_path)
            .with_context(|| "non canonical current binary path")?;
        let bin_dir = bin_path.parent().expect("invalid current binary dir");
        if self.paths.this_version != bin_dir {
            info!(
                "move BV files from {} to install path {}",
                bin_dir.to_string_lossy(),
                self.paths.this_version.to_string_lossy()
            );
            fs::create_dir_all(&self.paths.install_path).expect("failed to create install path");
            let move_opt = fs_extra::dir::CopyOptions {
                overwrite: true,
                skip_exist: false,
                buffer_size: 64000,
                copy_inside: true,
                content_only: false,
                depth: 0,
            };
            fs_extra::dir::move_dir(bin_dir, &self.paths.this_version, &move_opt)
                .with_context(|| "failed to move files to install path")?;
        }
        Ok(())
    }

    async fn handle_broken_installation(&self, err: Error) -> Result<()> {
        self.blacklist_this_version()?;

        match self.backup_status {
            BackupStatus::Done(_) => {
                // TODO: try to send install failed status to the backend
                self.rollback().await?;
                bail!("installation failed with: {err}, but rolled back to previous version")
            }
            BackupStatus::ThisIsRollback => {
                // TODO: try to send rollback failed status to the backend
                bail!("rollback failed - host needs manual fix: {err}")
            }
            BackupStatus::NothingToBackup => {
                // TODO: try to send install failed status to the backend
                bail!("installation failed: {err}");
            }
        }
    }

    fn is_blacklisted(&self, version: &str) -> Result<bool> {
        Ok(self.paths.blacklist.exists()
            && fs::read_to_string(&self.paths.blacklist)
                .with_context(|| "failed to read blacklist")?
                .contains(version))
    }

    fn backup_running_version(&mut self) -> Result<()> {
        if let Some(running_version) = self.get_running_version()? {
            if self.is_blacklisted(&running_version)? {
                self.backup_status = BackupStatus::ThisIsRollback;
            } else {
                info!("backup previously installed BV {running_version}");
                let _ = fs::remove_file(&self.paths.backup);
                std::os::unix::fs::symlink(
                    fs::read_link(&self.paths.current)
                        .with_context(|| "invalid current version link")?,
                    &self.paths.backup,
                )
                .with_context(|| "failed to backup running version for rollback")?;
                self.backup_status = BackupStatus::Done(running_version);
            }
        } else {
            self.backup_status = BackupStatus::NothingToBackup;
        }
        Ok(())
    }

    fn get_running_version(&self) -> Result<Option<String>> {
        if self.paths.current.exists() {
            // get running version if any
            let current_path_unlinked = self
                .paths
                .current
                .read_link()
                .with_context(|| "invalid current version link")?;
            Ok(current_path_unlinked
                .file_name()
                .and_then(|v| v.to_str().map(|v| v.to_owned())))
        } else {
            Ok(None)
        }
    }

    fn preinstall(&mut self) -> Result<()> {
        self.move_bundle_to_install_path(
            env::current_exe().with_context(|| "failed to get current binary path")?,
        )?;
        self.backup_running_version()
    }

    async fn install(&mut self) -> Result<()> {
        self.prepare_running().await?;
        self.install_this_version()?;
        self.restart_and_reenable_blockvisor().await?;
        self.health_check().await
    }

    async fn prepare_running(&mut self) -> Result<()> {
        if let BackupStatus::Done(_) = self.backup_status {
            info!("prepare running BV for update");
            let timestamp = self.timer.now();
            let expired = || {
                let now = self.timer.now();
                let duration = now.duration_since(timestamp);
                duration > PREPARE_FOR_UPDATE_TIMEOUT
            };
            loop {
                match with_retry!(self
                    .bv_client
                    .start_update(bv_pb::StartUpdateRequest::default()))
                {
                    Ok(resp) => {
                        let status = resp.into_inner().status();
                        if status == bv_pb::ServiceStatus::Updating {
                            break;
                        } else if expired() {
                            bail!("prepare running BV for update failed, BV start_update respond with {status:?}");
                        }
                    }
                    Err(err) => {
                        if expired() {
                            bail!("prepare running BV for update failed, BV start_update respond with {err}");
                        }
                    }
                }
                self.timer.sleep(BV_CHECK_INTERVAL);
            }
        }
        Ok(())
    }

    fn install_this_version(&self) -> Result<()> {
        info!("update system symlinks");
        // switch current to this version
        let _ = fs::remove_file(&self.paths.current);
        std::os::unix::fs::symlink(&self.paths.this_version, &self.paths.current)
            .with_context(|| "failed to switch current version")?;

        let symlink_all = |src, dst: &PathBuf| {
            for entry in self
                .paths
                .current
                .join(src)
                .read_dir()
                .with_context(|| format!("failed to get list of installed files in {src}"))?
            {
                let entry = entry.with_context(|| format!("failed to get file entry in {src}"))?;
                let link_path = dst.join(entry.file_name());
                let _ = fs::remove_file(&link_path);
                std::os::unix::fs::symlink(entry.path(), &link_path).with_context(|| {
                    format!(
                        "failed to link {} to {}",
                        entry.path().to_string_lossy(),
                        link_path.to_string_lossy()
                    )
                })?;
            }
            Ok(())
        };

        symlink_all(BLOCKVISOR_BIN, &self.paths.system_bin)?;
        symlink_all(FC_BIN, &self.paths.system_bin)?;
        symlink_all(BLOCKVISOR_SERVICES, &self.paths.system_services)
    }

    async fn restart_and_reenable_blockvisor(&self) -> Result<()> {
        self.bv_service.reload().await?;
        self.bv_service.stop().await?;
        self.migrate_bv_data()
            .with_context(|| "failed to migrate bv data to new version")?;
        self.bv_service.start().await?;
        self.bv_service.enable().await?;
        Ok(())
    }

    fn migrate_bv_data(&self) -> Result<()> {
        // backup state/config and migrate to new version

        // backup blockvisor config since new version may modify/break it
        fs::copy(
            self.paths.bv_root.join(CONFIG_PATH),
            self.paths.current.join(BLOCKVISOR_CONFIG),
        )?;
        Ok(())
    }

    async fn health_check(&mut self) -> Result<()> {
        info!("verify newly installed BV");
        let timestamp = self.timer.now();
        let expired = || {
            let now = self.timer.now();
            let duration = now.duration_since(timestamp);
            duration > HEALTH_CHECK_TIMEOUT
        };
        loop {
            match with_retry!(self.bv_client.health(bv_pb::HealthRequest::default())) {
                Ok(resp) => {
                    let status = resp.into_inner().status();
                    if status == bv_pb::ServiceStatus::Ok {
                        break;
                    } else if expired() {
                        bail!(
                            "installed BV health check failed, BV health respond with {status:?}"
                        );
                    }
                }
                Err(err) => {
                    if expired() {
                        bail!("installed BV health check failed, BV health respond with {err}");
                    }
                }
            }
            self.timer.sleep(BV_CHECK_INTERVAL);
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        // stop broken version first
        self.bv_service
            .stop()
            .await
            .with_context(|| "failed to stop broken installation - can't continue rollback")?;
        self.rollback_bv_data()
            .with_context(|| "failed to rollback BV data")?;
        let backup_installer = self.paths.backup.join(INSTALLER_BIN);
        ensure!(backup_installer.exists(), "no backup found");
        let status_code = Command::new(backup_installer)
            .status()
            .with_context(|| "failed to launch backup installer")?
            .code()
            .with_context(|| "failed to get backup installer exit status code")?;
        ensure!(
            status_code == 0,
            "backup installer failed with exit code {status_code}"
        );
        Ok(())
    }

    fn rollback_bv_data(&self) -> Result<()> {
        // rollback state/config

        // rollback blockvisor config
        fs::copy(
            self.paths.backup.join(BLOCKVISOR_CONFIG),
            self.paths.bv_root.join(CONFIG_PATH),
        )?;
        Ok(())
    }

    fn blacklist_this_version(&self) -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(&self.paths.blacklist)?;
        writeln!(file, "{THIS_VERSION}")
            .with_context(|| "install failed, but can't blacklist broken version")
    }

    fn cleanup(&self) -> Result<()> {
        info!("cleanup old BV files:");
        self.cleanup_bv_data_backup()?;
        let persistent = [
            &self.paths.blacklist,
            &self.paths.current,
            &self.paths.this_version,
        ];
        for entry in self
            .paths
            .install_path
            .read_dir()
            .with_context(|| "failed to get cleanup list")?
        {
            let entry = entry.with_context(|| "failed to get cleanup list entry")?;

            if !persistent.contains(&&entry.path()) {
                debug!("remove {}", entry.path().to_string_lossy());
                let _ = if entry.path().is_dir() {
                    fs::remove_dir_all(entry.path())
                } else {
                    fs::remove_file(entry.path())
                }
                // do not interrupt cleanup on errors
                .map_err(|err| {
                    let path = entry.path();
                    warn!(
                        "failed to cleanup after install, can't remove {} with: {}",
                        path.to_string_lossy(),
                        err
                    )
                });
            }
        }
        Ok(())
    }

    fn cleanup_bv_data_backup(&self) -> Result<()> {
        // cleanup state/config conversion remnants

        Ok(())
    }
}

async fn check_requirements() -> Result<()> {
    info!("checking BV {THIS_VERSION} requirements ...");
    check_cli_dependencies().await?;
    check_kernel_requirements()?;
    check_network_setup().await?;
    Ok(())
}

async fn check_cli_dependencies() -> Result<()> {
    // smoke test for all CLI tools used in BV
    for cmd in ["pigz", "tar", "fallocate", "systemctl"] {
        bv_utils::cmd::run_cmd(cmd, ["--version"]).await?;
    }
    for cmd in ["tmux", "ip", "mkfs.ext4"] {
        bv_utils::cmd::run_cmd(cmd, ["-V"]).await?;
    }
    Ok(())
}

fn check_kernel_requirements() -> Result<()> {
    const MIN_KERNEL_VERSION: Version = Version::new(4, 14, 0);
    const MAX_KERNEL_VERSION: Version = Version::new(6, 0, 0);
    let mut sys = System::new_all();
    sys.refresh_all();
    let kernel_version = Version::parse(
        &sys.kernel_version()
            .ok_or_else(|| anyhow!("can't read kernel version"))?,
    )?;
    if kernel_version < MIN_KERNEL_VERSION || kernel_version >= MAX_KERNEL_VERSION {
        bail!("supported kernel versions are >={MIN_KERNEL_VERSION} and <{MAX_KERNEL_VERSION}, but {kernel_version} found")
    }
    Ok(())
}

async fn check_network_setup() -> Result<()> {
    bv_utils::cmd::run_cmd("ip", ["link", "show", BRIDGE_IFACE])
        .await
        .with_context(|| format!("bridge interface '{BRIDGE_IFACE}' not configured"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::bv_pb;
    use crate::utils;
    use crate::utils::tests::test_channel;
    use anyhow::anyhow;
    use assert_fs::TempDir;
    use bv_utils::timer::MockTimer;
    use mockall::*;
    use std::ops::Add;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::Arc;
    use std::thread::sleep;
    use std::time::Instant;
    use tonic::Response;

    mock! {
        pub TestBV {}

        #[tonic::async_trait]
        impl bv_pb::blockvisor_server::Blockvisor for TestBV {
            async fn health(
                &self,
                request: tonic::Request<bv_pb::HealthRequest>,
            ) -> Result<tonic::Response<bv_pb::HealthResponse>, tonic::Status>;
            async fn start_update(
                &self,
                request: tonic::Request<bv_pb::StartUpdateRequest>,
            ) -> Result<tonic::Response<bv_pb::StartUpdateResponse>, tonic::Status>;
            async fn create_node(
                &self,
                request: tonic::Request<bv_pb::CreateNodeRequest>,
            ) -> Result<tonic::Response<bv_pb::CreateNodeResponse>, tonic::Status>;
            async fn upgrade_node(
                &self,
                request: tonic::Request<bv_pb::UpgradeNodeRequest>,
            ) -> Result<tonic::Response<bv_pb::UpgradeNodeResponse>, tonic::Status>;
            async fn delete_node(
                &self,
                request: tonic::Request<bv_pb::DeleteNodeRequest>,
            ) -> Result<tonic::Response<bv_pb::DeleteNodeResponse>, tonic::Status>;
            async fn start_node(
                &self,
                request: tonic::Request<bv_pb::StartNodeRequest>,
            ) -> Result<tonic::Response<bv_pb::StartNodeResponse>, tonic::Status>;
            async fn stop_node(
                &self,
                request: tonic::Request<bv_pb::StopNodeRequest>,
            ) -> Result<tonic::Response<bv_pb::StopNodeResponse>, tonic::Status>;
            async fn get_nodes(
                &self,
                request: tonic::Request<bv_pb::GetNodesRequest>,
            ) -> Result<tonic::Response<bv_pb::GetNodesResponse>, tonic::Status>;
            async fn get_node_status(
                &self,
                request: tonic::Request<bv_pb::GetNodeStatusRequest>,
            ) -> Result<tonic::Response<bv_pb::GetNodeStatusResponse>, tonic::Status>;
            async fn get_node_logs(
                &self,
                request: tonic::Request<bv_pb::GetNodeLogsRequest>,
            ) -> Result<tonic::Response<bv_pb::GetNodeLogsResponse>, tonic::Status>;
            async fn get_babel_logs(
                &self,
                request: tonic::Request<bv_pb::GetBabelLogsRequest>,
            ) -> Result<tonic::Response<bv_pb::GetBabelLogsResponse>, tonic::Status>;
            async fn get_node_keys(
                &self,
                request: tonic::Request<bv_pb::GetNodeKeysRequest>,
            ) -> Result<tonic::Response<bv_pb::GetNodeKeysResponse>, tonic::Status>;
            async fn get_node_id_for_name(
                &self,
                request: tonic::Request<bv_pb::GetNodeIdForNameRequest>,
            ) -> Result<tonic::Response<bv_pb::GetNodeIdForNameResponse>, tonic::Status>;
            async fn list_capabilities(
                &self,
                request: tonic::Request<bv_pb::ListCapabilitiesRequest>,
            ) -> Result<tonic::Response<bv_pb::ListCapabilitiesResponse>, tonic::Status>;
            async fn blockchain(
                &self,
                request: tonic::Request<bv_pb::BlockchainRequest>,
            ) -> Result<tonic::Response<bv_pb::BlockchainResponse>, tonic::Status>;
            async fn get_node_metrics(
                &self,
                request: tonic::Request<bv_pb::GetNodeMetricsRequest>,
            ) -> Result<tonic::Response<bv_pb::GetNodeMetricsResponse>, tonic::Status>;
        }
    }

    mock! {
        pub TestBvService {}

        #[async_trait]
        impl BvService for TestBvService {
            async fn reload(&self) -> Result<()>;
            async fn stop(&self) -> Result<()>;
            async fn start(&self) -> Result<()>;
            async fn enable(&self) -> Result<()>;
        }
    }

    fn touch_file(path: &PathBuf) -> std::io::Result<fs::File> {
        fs::OpenOptions::new().create(true).write(true).open(path)
    }

    /// Common staff to setup for all tests like sut (installer in that case),
    /// path to root dir used in test, instance of AsyncPanicChecker to make sure that all panics
    /// from other threads will be propagated.
    struct TestEnv {
        tmp_root: PathBuf,
        _async_panic_checker: utils::tests::AsyncPanicChecker,
    }

    impl TestEnv {
        fn new() -> Result<Self> {
            let tmp_root = TempDir::new()?.to_path_buf();
            let _ = fs::create_dir_all(&tmp_root);

            Ok(Self {
                tmp_root,
                _async_panic_checker: Default::default(),
            })
        }

        fn start_test_server(&self, bv_mock: MockTestBV) -> utils::tests::TestServer {
            utils::tests::start_test_server(
                self.tmp_root.join("test_socket"),
                bv_pb::blockvisor_server::BlockvisorServer::new(bv_mock),
            )
        }

        fn build_installer(
            &self,
            timer: MockTimer,
            bv_service: MockTestBvService,
        ) -> Installer<MockTimer, MockTestBvService> {
            Installer::internal_new(
                timer,
                bv_service,
                &self.tmp_root,
                test_channel(&self.tmp_root),
            )
        }
    }

    #[tokio::test]
    async fn test_prepare_running_none() -> Result<()> {
        let test_env = TestEnv::new()?;

        let mut installer = test_env.build_installer(MockTimer::new(), MockTestBvService::new());
        installer.backup_status = BackupStatus::NothingToBackup;
        installer.prepare_running().await?;
        installer.backup_status = BackupStatus::ThisIsRollback;
        installer.prepare_running().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_prepare_running_ok() -> Result<()> {
        let test_env = TestEnv::new()?;

        let mut bv_mock = MockTestBV::new();
        bv_mock.expect_start_update().once().returning(|_| {
            let reply = bv_pb::StartUpdateResponse {
                status: bv_pb::ServiceStatus::Updating.into(),
            };
            Ok(Response::new(reply))
        });
        let mut timer_mock = MockTimer::new();
        timer_mock.expect_sleep().returning(|_| ());
        let now = Instant::now();
        timer_mock.expect_now().returning(move || now);
        let server = test_env.start_test_server(bv_mock);
        let mut installer = test_env.build_installer(timer_mock, MockTestBvService::new());
        installer.backup_status = BackupStatus::Done(THIS_VERSION.to_owned());
        installer.prepare_running().await?;
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_prepare_running_timeout() -> Result<()> {
        let test_env = TestEnv::new()?;
        let mut bv_mock = MockTestBV::new();
        let update_start_called = Arc::new(AtomicBool::new(false));
        let update_start_called_flag = update_start_called.clone();
        bv_mock.expect_start_update().once().returning(move |_| {
            let reply = bv_pb::StartUpdateResponse {
                status: bv_pb::ServiceStatus::Ok.into(),
            };
            update_start_called_flag.store(true, Relaxed);
            Ok(Response::new(reply))
        });
        let mut timer_mock = MockTimer::new();
        let now = Instant::now();
        timer_mock.expect_now().once().returning(move || now);
        timer_mock
            .expect_sleep()
            .with(predicate::eq(BV_CHECK_INTERVAL))
            .returning(|_| ());
        timer_mock.expect_now().returning(move || {
            if update_start_called.load(Relaxed) {
                now.add(PREPARE_FOR_UPDATE_TIMEOUT)
                    .add(Duration::from_secs(1))
            } else {
                now
            }
        });
        let server = test_env.start_test_server(bv_mock);
        let mut installer = test_env.build_installer(timer_mock, MockTestBvService::new());
        installer.backup_status = BackupStatus::Done(THIS_VERSION.to_owned());
        installer.prepare_running().await.unwrap_err();
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_health_check_ok() -> Result<()> {
        let test_env = TestEnv::new()?;

        let mut bv_mock = MockTestBV::new();
        bv_mock.expect_health().times(2).returning(|_| {
            let reply = bv_pb::HealthResponse {
                status: bv_pb::ServiceStatus::Ok.into(),
            };
            Ok(Response::new(reply))
        });
        let mut timer_mock = MockTimer::new();
        timer_mock.expect_now().returning(Instant::now);
        let server = test_env.start_test_server(bv_mock);
        let mut client = BlockvisorClient::new(test_channel(&test_env.tmp_root));
        while client
            .health(bv_pb::HealthRequest::default())
            .await
            .is_err()
        {
            sleep(Duration::from_millis(10));
        }

        test_env
            .build_installer(timer_mock, MockTestBvService::new())
            .health_check()
            .await?;
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_health_check_timeout() -> Result<()> {
        let test_env = TestEnv::new()?;
        let mut bv_mock = MockTestBV::new();
        bv_mock.expect_health().returning(|_| {
            let reply = bv_pb::HealthResponse {
                status: bv_pb::ServiceStatus::Updating.into(),
            };
            Ok(Response::new(reply))
        });
        let mut timer_mock = MockTimer::new();
        let now = Instant::now();
        timer_mock.expect_now().once().returning(move || now);
        timer_mock
            .expect_sleep()
            .with(predicate::eq(BV_CHECK_INTERVAL))
            .returning(|_| ());
        timer_mock
            .expect_now()
            .once()
            .returning(move || now.add(HEALTH_CHECK_TIMEOUT).add(Duration::from_secs(1)));
        let server = test_env.start_test_server(bv_mock);
        let mut client = BlockvisorClient::new(test_channel(&test_env.tmp_root));
        while client
            .health(bv_pb::HealthRequest::default())
            .await
            .is_err()
        {
            sleep(Duration::from_millis(10));
        }

        test_env
            .build_installer(timer_mock, MockTestBvService::new())
            .health_check()
            .await
            .unwrap_err();
        server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_backup_running_version() -> Result<()> {
        let test_env = TestEnv::new()?;
        let mut installer = test_env.build_installer(MockTimer::new(), MockTestBvService::new());

        fs::create_dir_all(&installer.paths.install_path)?;
        installer.backup_running_version()?;
        assert_eq!(BackupStatus::NothingToBackup, installer.backup_status);

        fs::create_dir_all(&installer.paths.this_version)?;
        std::os::unix::fs::symlink(&installer.paths.this_version, &installer.paths.current)?;
        installer.backup_running_version()?;
        assert_eq!(
            BackupStatus::Done(THIS_VERSION.to_owned()),
            installer.backup_status
        );
        assert_eq!(
            &installer.paths.this_version,
            &installer.paths.backup.read_link()?
        );

        installer.blacklist_this_version()?;
        installer.backup_running_version()?;
        assert_eq!(BackupStatus::ThisIsRollback, installer.backup_status);

        Ok(())
    }

    #[tokio::test]
    async fn test_move_bundle_to_install_path() -> Result<()> {
        let test_env = TestEnv::new()?;
        let installer = test_env.build_installer(MockTimer::new(), MockTestBvService::new());
        let bundle_path = test_env.tmp_root.join("bundle");

        installer
            .move_bundle_to_install_path(bundle_path.join("installer"))
            .unwrap_err();

        fs::create_dir_all(bundle_path.join("some_dir/with_subdir"))?;
        touch_file(&bundle_path.join("installer"))?;
        touch_file(&bundle_path.join("some_file"))?;
        touch_file(&bundle_path.join("some_dir/sub_file"))?;

        installer.move_bundle_to_install_path(bundle_path.join("installer"))?;
        installer.move_bundle_to_install_path(installer.paths.this_version.join("installer"))?;

        assert!(installer.paths.this_version.join("installer").exists());
        assert!(installer.paths.this_version.join("some_file").exists());
        assert!(installer
            .paths
            .this_version
            .join("some_dir/with_subdir")
            .exists());
        assert!(installer
            .paths
            .this_version
            .join("some_dir/sub_file")
            .exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_install_this_version() -> Result<()> {
        let test_env = TestEnv::new()?;
        let installer = test_env.build_installer(MockTimer::new(), MockTestBvService::new());

        installer.install_this_version().unwrap_err();

        let this_path = &installer.paths.this_version;
        fs::create_dir_all(this_path)?;
        installer.install_this_version().unwrap_err();

        fs::create_dir_all(this_path.join(BLOCKVISOR_BIN))?;
        fs::create_dir_all(this_path.join(BLOCKVISOR_SERVICES))?;
        fs::create_dir_all(this_path.join(FC_BIN))?;
        installer.install_this_version()?;

        touch_file(&this_path.join(BLOCKVISOR_BIN).join("some_bin"))?;
        touch_file(&this_path.join(BLOCKVISOR_SERVICES).join("some_service"))?;
        touch_file(&this_path.join(FC_BIN).join("firecracker"))?;
        installer.install_this_version().unwrap_err();

        fs::create_dir_all(&installer.paths.system_bin)?;
        fs::create_dir_all(&installer.paths.system_services)?;
        installer.install_this_version()?;

        assert!(installer.paths.system_bin.join("some_bin").exists());
        assert!(installer.paths.system_bin.join("firecracker").exists());
        assert!(installer
            .paths
            .system_services
            .join("some_service")
            .exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_broken_installation() -> Result<()> {
        let test_env = TestEnv::new()?;
        let mut service_mock = MockTestBvService::new();
        service_mock.expect_reload().return_once(|| Ok(()));
        service_mock.expect_stop().return_once(|| Ok(()));
        service_mock.expect_stop().return_once(|| Ok(()));
        service_mock.expect_enable().return_once(|| Ok(()));
        let mut installer = test_env.build_installer(MockTimer::new(), service_mock);

        installer.backup_status = BackupStatus::ThisIsRollback;
        installer
            .handle_broken_installation(anyhow!("error"))
            .await
            .unwrap_err();
        assert!(!installer.is_blacklisted(THIS_VERSION)?);

        fs::create_dir_all(&installer.paths.install_path)?;
        installer.backup_status = BackupStatus::NothingToBackup;
        installer
            .handle_broken_installation(anyhow!("error"))
            .await
            .unwrap_err();
        assert!(installer.is_blacklisted(THIS_VERSION)?);

        fs::create_dir_all(&installer.paths.backup)?;
        fs::create_dir_all(installer.paths.bv_root.join("etc"))?;
        fs::File::create(installer.paths.backup.join(BLOCKVISOR_CONFIG))?;
        {
            // create dummy installer that will touch test file as a proof it was called
            let mut backup_installer = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o770)
                .open(installer.paths.backup.join(INSTALLER_BIN))?;
            writeln!(backup_installer, "#!/bin/sh")?;
            writeln!(
                backup_installer,
                "touch {}",
                test_env.tmp_root.join("dummy_installer").to_str().unwrap()
            )?;
            writeln!(backup_installer, "exit 1")?;
        }
        let _ = fs::remove_file(test_env.tmp_root.join("dummy_installer"));
        installer.backup_status = BackupStatus::Done(THIS_VERSION.to_owned());
        installer
            .handle_broken_installation(anyhow!("error"))
            .await
            .unwrap_err();
        assert!(test_env.tmp_root.join("dummy_installer").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_cleanup() -> Result<()> {
        let test_env = TestEnv::new()?;
        let installer = test_env.build_installer(MockTimer::new(), MockTestBvService::new());

        // cant cleanup non existing dir nothing
        installer.cleanup().unwrap_err();

        fs::create_dir_all(&installer.paths.install_path)?;

        // cleanup empty dir
        installer.cleanup()?;

        touch_file(&installer.paths.install_path.join("some_file"))?;
        fs::create_dir_all(installer.paths.install_path.join("some_dir"))?;
        touch_file(
            &installer
                .paths
                .install_path
                .join("some_dir")
                .join("another_file"),
        )?;
        std::os::unix::fs::symlink(
            installer.paths.install_path.join("some_dir"),
            installer.paths.install_path.join("dir_link"),
        )?;
        std::os::unix::fs::symlink(
            installer.paths.install_path.join("some_file"),
            installer.paths.install_path.join("file_link"),
        )?;
        fs::create_dir_all(&installer.paths.this_version)?;
        std::os::unix::fs::symlink(&installer.paths.this_version, &installer.paths.current)?;
        touch_file(&installer.paths.blacklist)?;
        installer.cleanup()?;

        let mut remaining = installer
            .paths
            .install_path
            .read_dir()?
            .map(|res| res.map(|e| e.path()))
            .collect::<Result<Vec<_>, std::io::Error>>()?;
        remaining.sort();
        assert_eq!(3, remaining.len());
        assert!(remaining.contains(&installer.paths.blacklist));
        assert!(remaining.contains(&installer.paths.current));
        assert!(remaining.contains(&installer.paths.this_version));

        Ok(())
    }
}
