use crate::server::bv_pb::blockvisor_client::BlockvisorClient;
use crate::server::{bv_pb, BLOCKVISOR_SERVICE_URL};
use crate::utils::get_process_pid;
use anyhow::{bail, ensure, Context, Error, Result};
use std::io::Write;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use std::{env, fs};
use tonic::transport::Channel;

const SYSTEM_SERVICES: &str = "etc/systemd/system";
const SYSTEM_BIN: &str = "usr/bin";
const INSTALL_PATH: &str = "opt/blockvisor";
const BLACKLIST: &str = "blacklist";
const CURRENT_LINK: &str = "current";
const BACKUP_LINK: &str = "backup";
const INSTALLER_BIN: &str = "installer";
const FC_BIN: &str = "firecracker/bin";
const BLOCKVISOR_BIN: &str = "blockvisor/bin";
const BLOCKVISOR_SERVICES: &str = "blockvisor/services";
const THIS_VERSION: &str = env!("CARGO_PKG_VERSION");
const BV_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const BV_REQ_TIMEOUT: Duration = Duration::from_secs(1);
const BV_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const PREPARE_FOR_UPDATE_TIMEOUT: Duration = Duration::from_secs(180);

struct InstallerPaths {
    system_services: PathBuf,
    system_bin: PathBuf,
    install_path: PathBuf,
    current: PathBuf,
    this_version: PathBuf,
    backup: PathBuf,
    blacklist: PathBuf,
}

impl<T: Timer> Default for Installer<T> {
    fn default() -> Self {
        Self::new(
            crate::env::ROOT_DIR.clone(),
            Channel::from_static(BLOCKVISOR_SERVICE_URL)
                .timeout(BV_REQ_TIMEOUT)
                .connect_timeout(BV_CONNECT_TIMEOUT)
                .connect_lazy(),
        )
    }
}

/// Time abstraction for better testing.
pub trait Timer {
    fn now() -> Instant;
    fn sleep(duration: Duration);
}

#[derive(Debug, PartialEq)]
enum BackupStatus {
    Done,
    NothingToBackup,
    ThisIsRollback,
}

pub struct Installer<T: Timer> {
    paths: InstallerPaths,
    bv_client: BlockvisorClient<Channel>,
    phantom: PhantomData<T>,
}

impl<T: Timer> Installer<T> {
    pub async fn run(mut self) -> Result<()> {
        if self.is_blacklisted(THIS_VERSION)? {
            bail!("BV {THIS_VERSION} is on a blacklist - can't install")
        }
        println!("installing BV {THIS_VERSION}...");

        match self.preinstall() {
            Ok(backup_status) => {
                match self.install().await {
                    Ok(_) => {
                        // try cleanup after install, but cleanup result should not affect exit code
                        let _ = self
                            .cleanup() // do not interrupt cleanup on errors
                            .map_err(|err| println!("failed to cleanup after install with: {err}"));
                        Ok(())
                    }
                    Err(err) => self.handle_broken_installation(backup_status, err),
                }
            }
            Err(err) => {
                // TODO: try to send install failed status to the backend
                Err(err)
            }
        }
    }

    fn new(root: PathBuf, bv_channel: Channel) -> Self {
        let install_path = root.join(INSTALL_PATH);
        let current = install_path.join(CURRENT_LINK);
        let this_version = install_path.join(THIS_VERSION);
        let backup = install_path.join(BACKUP_LINK);
        let blacklist = install_path.join(BLACKLIST);

        Self {
            paths: InstallerPaths {
                system_services: root.join(SYSTEM_SERVICES),
                system_bin: root.join(SYSTEM_BIN),
                install_path,
                current,
                this_version,
                backup,
                blacklist,
            },
            bv_client: BlockvisorClient::new(bv_channel),
            phantom: Default::default(),
        }
    }

    fn move_bundle_to_install_path(&self, current_exe_path: PathBuf) -> Result<()> {
        let bin_path = fs::canonicalize(current_exe_path)
            .with_context(|| "non canonical current binary path")?;
        let bin_dir = bin_path.parent().expect("invalid current binary dir");
        if self.paths.this_version != bin_dir {
            println!(
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

    fn handle_broken_installation(&self, backup_status: BackupStatus, err: Error) -> Result<()> {
        self.blacklist_this_version()?;

        match backup_status {
            BackupStatus::Done => {
                // TODO: try to send install failed status to the backend
                self.rollback()?;
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

    fn backup_running_version(&self) -> Result<BackupStatus> {
        if let Some(running_version) = self.get_running_version()? {
            if self.is_blacklisted(running_version.as_str())? {
                Ok(BackupStatus::ThisIsRollback)
            } else {
                println!("backup previously installed BV {running_version}");
                let _ = fs::remove_file(&self.paths.backup);
                std::os::unix::fs::symlink(
                    fs::read_link(&self.paths.current)
                        .with_context(|| "invalid current version link")?,
                    &self.paths.backup,
                )
                .with_context(|| "failed to backup running version for rollback")?;
                Ok(BackupStatus::Done)
            }
        } else {
            Ok(BackupStatus::NothingToBackup)
        }
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

    fn preinstall(&self) -> Result<BackupStatus> {
        self.move_bundle_to_install_path(
            env::current_exe().with_context(|| "failed to get current binary path")?,
        )?;
        self.backup_running_version()
    }

    async fn install(&mut self) -> Result<()> {
        self.prepare_running().await?;
        self.install_this_version()?;
        Self::restart_blockvisor()?;
        self.health_check().await
    }

    async fn prepare_running(&mut self) -> Result<()> {
        if !self.paths.current.exists() //fresh installation
            || get_process_pid( // rollback - launched by another installer from self.paths.current
                INSTALLER_BIN,
                &self.paths.current.join(INSTALLER_BIN).to_string_lossy(),
            )
            .is_err()
        {
            return Ok(());
        }
        println!("prepare running BV for update");
        let timestamp = T::now();
        let expired = || {
            let now = T::now();
            let duration = now.duration_since(timestamp);
            duration > PREPARE_FOR_UPDATE_TIMEOUT
        };
        loop {
            match self
                .bv_client
                .start_update(bv_pb::StartUpdateRequest::default())
                .await
            {
                Ok(resp) => {
                    let status = resp.into_inner().status;
                    if status == bv_pb::ServiceStatus::Updating as i32 {
                        break;
                    } else if expired() {
                        bail!("prepare running BV for update failed, BV start_update respond with {status}");
                    }
                }
                Err(err) => {
                    if expired() {
                        bail!("prepare running BV for update failed, BV start_update respond with {err}");
                    }
                }
            }
            T::sleep(BV_CHECK_INTERVAL);
        }
        Ok(())
    }

    fn install_this_version(&self) -> Result<()> {
        println!("update system symlinks");
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

    fn restart_blockvisor() -> Result<()> {
        println!("request blockvisor service restart (systemctl restart blockvisor)");
        let status_code = Command::new("systemctl")
            .args(["restart", "blockvisor"])
            .status()
            .with_context(|| "failed to restart blockvisor service")?
            .code()
            .with_context(|| "failed to get systemctl exit status code")?;
        ensure!(
            status_code == 0,
            "blockvisor service restart failed with exit code {status_code}"
        );
        Ok(())
    }

    async fn health_check(&mut self) -> Result<()> {
        println!("verify newly installed BV");
        let timestamp = T::now();
        let expired = || {
            let now = T::now();
            let duration = now.duration_since(timestamp);
            duration > HEALTH_CHECK_TIMEOUT
        };
        loop {
            match self.bv_client.health(bv_pb::HealthRequest::default()).await {
                Ok(resp) => {
                    let status = resp.into_inner().status;
                    if status == bv_pb::ServiceStatus::Ok as i32 {
                        break;
                    } else if expired() {
                        bail!("installed BV health check failed, BV health respond with {status}");
                    }
                }
                Err(err) => {
                    if expired() {
                        bail!("installed BV health check failed, BV health respond with {err}");
                    }
                }
            }
            T::sleep(BV_CHECK_INTERVAL);
        }
        Ok(())
    }

    fn rollback(&self) -> Result<()> {
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
        println!("cleanup old BV files:");
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
                println!("remove {}", entry.path().to_string_lossy());
                let _ = if entry.path().is_dir() {
                    fs::remove_dir_all(entry.path())
                } else {
                    fs::remove_file(entry.path())
                }
                // do not interrupt cleanup on errors
                .map_err(|err| {
                    let path = entry.path();
                    println!(
                        "failed to cleanup after install, can't remove {} with: {}",
                        path.to_string_lossy(),
                        err
                    )
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::bv_pb;
    use anyhow::anyhow;
    use assert_fs::TempDir;
    use mockall::*;
    use serial_test::serial;
    use std::ops::Add;
    use std::os::unix::fs::OpenOptionsExt;
    use tokio::net::UnixStream;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::{Endpoint, Server, Uri};
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
        pub TestTimer {}

        impl Timer for TestTimer {
            fn now() -> Instant;
            fn sleep(duration: Duration);
        }
    }

    fn touch_file(path: &PathBuf) -> std::io::Result<fs::File> {
        fs::OpenOptions::new().create(true).write(true).open(path)
    }

    async fn test_server(tmp_root: &PathBuf, bv_mock: MockTestBV) -> Result<()> {
        let socket_path = tmp_root.join("test_socket");
        let uds_stream =
            UnixListenerStream::new(tokio::net::UnixListener::bind(socket_path.clone())?);
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(bv_pb::blockvisor_server::BlockvisorServer::new(bv_mock))
            .serve_with_incoming(uds_stream)
            .await?;
        Ok(())
    }

    fn test_channel(tmp_root: &PathBuf) -> Channel {
        let socket_path = tmp_root.join("test_socket");
        Endpoint::try_from("http://[::]:50052")
            .unwrap()
            .timeout(Duration::from_secs(1))
            .connect_timeout(Duration::from_secs(1))
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                UnixStream::connect(socket_path.clone())
            }))
    }

    fn create_dummy_installer(path: &PathBuf) -> Result<()> {
        // create dummy installer that will sleep
        let _ = fs::create_dir_all(path);
        let mut installer = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o770)
            .open(path.join(INSTALLER_BIN))?;
        writeln!(installer, "#!/bin/sh")?;
        writeln!(installer, "sleep infinity")?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_prepare_running_none() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let _ = fs::create_dir_all(&tmp_root);
        let channel = test_channel(&tmp_root);

        let mut installer = Installer::<MockTestTimer>::new(tmp_root.clone(), channel);
        installer.prepare_running().await?;
        let _ = fs::create_dir_all(&installer.paths.current);
        installer.prepare_running().await?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_prepare_running_ok() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let _ = fs::create_dir_all(&tmp_root);
        let channel = test_channel(&tmp_root);

        let mut installer = Installer::<MockTestTimer>::new(tmp_root.clone(), channel);
        create_dummy_installer(&installer.paths.current)?;
        let mut dummy_installer =
            tokio::process::Command::new(&installer.paths.current.join(INSTALLER_BIN)).spawn()?;
        let mut bv_mock = MockTestBV::new();
        bv_mock.expect_start_update().once().returning(|_| {
            let reply = bv_pb::StartUpdateResponse {
                status: bv_pb::ServiceStatus::Updating.into(),
            };
            Ok(Response::new(reply))
        });
        let now_ctx = MockTestTimer::now_context();
        let now = Instant::now();
        now_ctx.expect().once().returning(move || now);

        let resp = tokio::select!(
            resp = installer.prepare_running() => resp,
            _ = test_server(&tmp_root, bv_mock) => Ok(()),
            _ = dummy_installer.wait() => Ok(()),
        );
        resp?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_prepare_running_timeout() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let _ = fs::create_dir_all(&tmp_root);
        let channel = test_channel(&tmp_root);

        let mut installer = Installer::<MockTestTimer>::new(tmp_root.clone(), channel);
        create_dummy_installer(&installer.paths.current)?;
        let mut dummy_installer =
            tokio::process::Command::new(&installer.paths.current.join(INSTALLER_BIN)).spawn()?;
        let mut bv_mock = MockTestBV::new();
        bv_mock.expect_start_update().once().returning(|_| {
            let reply = bv_pb::StartUpdateResponse {
                status: bv_pb::ServiceStatus::Ok.into(),
            };
            Ok(Response::new(reply))
        });
        let now_ctx = MockTestTimer::now_context();
        let sleep_ctx = MockTestTimer::sleep_context();
        let now = Instant::now();
        now_ctx.expect().once().returning(move || now);
        sleep_ctx
            .expect()
            .with(predicate::eq(BV_CHECK_INTERVAL))
            .returning(|_| ());
        now_ctx.expect().once().returning(move || {
            now.add(PREPARE_FOR_UPDATE_TIMEOUT)
                .add(Duration::from_secs(1))
        });

        let resp = tokio::select!(
            resp = installer.prepare_running() => resp,
            _ = test_server(&tmp_root, bv_mock) => Ok(()),
            _ = dummy_installer.wait() => Ok(()),
        );
        assert!(resp.is_err());
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_health_check_ok() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let _ = fs::create_dir_all(&tmp_root);
        let channel = test_channel(&tmp_root);

        let mut installer = Installer::<MockTestTimer>::new(tmp_root.clone(), channel);
        let mut bv_mock = MockTestBV::new();
        bv_mock.expect_health().once().returning(|_| {
            let reply = bv_pb::HealthResponse {
                status: bv_pb::ServiceStatus::Ok.into(),
            };
            Ok(Response::new(reply))
        });
        let now_ctx = MockTestTimer::now_context();
        now_ctx.expect().times(1).returning(move || Instant::now());

        let resp = tokio::select!(
            resp = installer.health_check() => resp,
            resp = test_server(&tmp_root, bv_mock) => resp,
        );
        resp?;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_health_check_timeout() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let _ = fs::create_dir_all(&tmp_root);
        let channel = test_channel(&tmp_root);

        let mut installer = Installer::<MockTestTimer>::new(tmp_root.clone(), channel);
        let mut bv_mock = MockTestBV::new();
        bv_mock.expect_health().returning(|_| {
            let reply = bv_pb::HealthResponse {
                status: bv_pb::ServiceStatus::Updating.into(),
            };
            Ok(Response::new(reply))
        });
        let now_ctx = MockTestTimer::now_context();
        let sleep_ctx = MockTestTimer::sleep_context();
        let now = Instant::now();
        now_ctx.expect().once().returning(move || now);
        sleep_ctx
            .expect()
            .with(predicate::eq(BV_CHECK_INTERVAL))
            .returning(|_| ());
        now_ctx
            .expect()
            .once()
            .returning(move || now.add(HEALTH_CHECK_TIMEOUT).add(Duration::from_secs(1)));

        let resp = tokio::select!(
            resp = installer.health_check() => resp,
            resp = test_server(&tmp_root, bv_mock) => resp,
        );
        assert!(resp.is_err());
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_backup_running_version() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let channel = test_channel(&tmp_root);
        let installer = Installer::<MockTestTimer>::new(tmp_root, channel);

        fs::create_dir_all(&installer.paths.install_path)?;
        assert_eq!(
            BackupStatus::NothingToBackup,
            installer.backup_running_version()?
        );

        fs::create_dir_all(&installer.paths.this_version)?;
        std::os::unix::fs::symlink(&installer.paths.this_version, &installer.paths.current)?;
        assert_eq!(BackupStatus::Done, installer.backup_running_version()?);
        assert_eq!(
            &installer.paths.this_version,
            &installer.paths.backup.read_link()?
        );

        installer.blacklist_this_version()?;
        assert_eq!(
            BackupStatus::ThisIsRollback,
            installer.backup_running_version()?
        );

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_move_bundle_to_install_path() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let bundle_path = tmp_root.join("bundle");
        let channel = test_channel(&tmp_root);
        let installer = Installer::<MockTestTimer>::new(tmp_root, channel);

        assert!(installer
            .move_bundle_to_install_path(bundle_path.join("installer"))
            .is_err());

        fs::create_dir_all(&bundle_path.join("some_dir/with_subdir"))?;
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
    #[serial]
    async fn test_install_this_version() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let channel = test_channel(&tmp_root);
        let installer = Installer::<MockTestTimer>::new(tmp_root, channel);

        assert!(installer.install_this_version().is_err());

        let this_path = &installer.paths.this_version;
        fs::create_dir_all(this_path)?;
        assert!(installer.install_this_version().is_err());

        fs::create_dir_all(this_path.join(BLOCKVISOR_BIN))?;
        fs::create_dir_all(this_path.join(BLOCKVISOR_SERVICES))?;
        fs::create_dir_all(this_path.join(FC_BIN))?;
        installer.install_this_version()?;

        touch_file(&this_path.join(BLOCKVISOR_BIN).join("some_bin"))?;
        touch_file(&this_path.join(BLOCKVISOR_SERVICES).join("some_service"))?;
        touch_file(&this_path.join(FC_BIN).join("firecracker"))?;
        assert!(installer.install_this_version().is_err());

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
    #[serial]
    async fn test_broken_installation() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let channel = test_channel(&tmp_root);
        let installer = Installer::<MockTestTimer>::new(tmp_root.clone(), channel);

        assert!(installer
            .handle_broken_installation(BackupStatus::ThisIsRollback, anyhow!("error"))
            .is_err());
        assert!(!installer.is_blacklisted(THIS_VERSION)?);

        fs::create_dir_all(&installer.paths.install_path)?;
        assert!(installer
            .handle_broken_installation(BackupStatus::NothingToBackup, anyhow!("error"))
            .is_err());
        assert!(installer.is_blacklisted(THIS_VERSION)?);

        fs::create_dir_all(&installer.paths.backup)?;
        {
            // create dummy installer that will touch test file as a proof it was called
            let mut backup_installer = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .mode(0o770)
                .open(&installer.paths.backup.join(INSTALLER_BIN))?;
            writeln!(backup_installer, "#!/bin/sh")?;
            writeln!(
                backup_installer,
                "touch {}",
                tmp_root.join("dummy_installer").to_str().unwrap()
            )?;
            writeln!(backup_installer, "exit 1")?;
        }
        let _ = fs::remove_file(tmp_root.join("dummy_installer"));
        assert!(installer
            .handle_broken_installation(BackupStatus::Done, anyhow!("error"))
            .is_err());
        assert!(tmp_root.join("dummy_installer").exists());

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_cleanup() -> Result<()> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let channel = test_channel(&tmp_root);
        let installer = Installer::<MockTestTimer>::new(tmp_root, channel);

        // cant cleanup non existing dir nothing
        assert!(installer.cleanup().is_err());

        fs::create_dir_all(&installer.paths.install_path)?;

        // cleanup empty dir
        installer.cleanup()?;

        touch_file(&installer.paths.install_path.join("some_file"))?;
        fs::create_dir_all(&installer.paths.install_path.join("some_dir"))?;
        touch_file(
            &installer
                .paths
                .install_path
                .join("some_dir")
                .join("another_file"),
        )?;
        std::os::unix::fs::symlink(
            &installer.paths.install_path.join("some_dir"),
            &installer.paths.install_path.join("dir_link"),
        )?;
        std::os::unix::fs::symlink(
            &installer.paths.install_path.join("some_file"),
            &installer.paths.install_path.join("file_link"),
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
