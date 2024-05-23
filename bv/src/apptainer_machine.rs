use crate::config::ApptainerConfig;
use crate::services::blockchain;
use crate::services::blockchain::ROOTFS_FILE;
use crate::utils::{get_process_pid, GetProcessIdError};
use crate::{node_context, node_state::NodeState, pal};
use async_trait::async_trait;
use babel_api::engine::{PosixSignal, DATA_DRIVE_MOUNT_POINT};
use bv_utils::cmd::run_cmd;
use bv_utils::system::{gracefully_terminate_process, is_process_running, kill_all_processes};
use bv_utils::with_retry;
use eyre::{anyhow, bail, Result};
use std::fmt::Debug;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, error};
use uuid::Uuid;

const ROOTFS_DIR: &str = "rootfs";
const DATA_DIR: &str = "data";
const JOURNAL_DIR: &str = "/run/systemd/journal";
const BABEL_BIN_NAME: &str = "babel";
const BABEL_KILL_TIMEOUT: Duration = Duration::from_secs(60);
const BABEL_START_TIMEOUT: Duration = Duration::from_secs(3);
const UMOUNT_RETRY_MAX: u32 = 2;
const UMOUNT_BACKOFF_BASE_MS: u64 = 1000;
const BABEL_BIN_PATH: &str = "/usr/bin/babel";
const APPTAINER_BIN_NAME: &str = "apptainer";

pub fn build_rootfs_dir(node_dir: &Path) -> PathBuf {
    node_dir.join(ROOTFS_DIR)
}

#[derive(Debug)]
pub struct ApptainerMachine {
    node_dir: PathBuf,
    babel_path: PathBuf,
    chroot_dir: PathBuf,
    data_dir: PathBuf,
    os_img_path: PathBuf,
    vm_id: Uuid,
    vm_name: String,
    ip: IpAddr,
    mask_bits: u8,
    gateway: IpAddr,
    requirements: babel_api::metadata::Requirements,
    config: ApptainerConfig,
}

pub async fn new(
    bv_root: &Path,
    gateway: IpAddr,
    mask_bits: u8,
    node_state: &NodeState,
    babel_path: PathBuf,
    config: ApptainerConfig,
) -> Result<ApptainerMachine> {
    let node_dir = node_context::build_node_dir(bv_root, node_state.id);
    let chroot_dir = node_dir.join(ROOTFS_DIR);
    fs::create_dir_all(&chroot_dir).await?;
    let data_dir = node_dir.join(DATA_DIR);
    fs::create_dir_all(&data_dir).await?;
    let os_img_path = node_dir.join(ROOTFS_FILE);
    if !os_img_path.exists() {
        fs::copy(
            blockchain::get_image_download_folder_path(bv_root, &node_state.image)
                .join(ROOTFS_FILE),
            &os_img_path,
        )
        .await?;
    }
    Ok(ApptainerMachine {
        node_dir,
        babel_path,
        chroot_dir,
        data_dir,
        os_img_path,
        vm_id: node_state.id,
        vm_name: node_state.name.clone(),
        ip: node_state.network_interface.ip,
        mask_bits,
        gateway,
        requirements: node_state.requirements.clone(),
        config,
    })
}

impl ApptainerMachine {
    pub async fn create(self) -> Result<Self> {
        let mut mounted = vec![];
        let vm = self.try_create(&mut mounted).await;
        if vm.is_err() {
            for mount_point in mounted {
                let mount_point = mount_point.to_string_lossy().to_string();
                if let Err(err) = with_retry!(
                    run_cmd("umount", [&mount_point]),
                    UMOUNT_RETRY_MAX,
                    UMOUNT_BACKOFF_BASE_MS
                ) {
                    error!("after create failed, can't umount {mount_point}: {err:#}")
                }
            }
        }
        vm
    }

    async fn try_create(self, mounted: &mut Vec<PathBuf>) -> Result<Self> {
        if !is_mounted(&self.chroot_dir).await? {
            run_cmd(
                "mount",
                [
                    self.os_img_path.clone().into_os_string(),
                    self.chroot_dir.clone().into_os_string(),
                ],
            )
            .await
            .map_err(|err| anyhow!("failed to mount '{}': {err:#}", self.os_img_path.display()))?;
            mounted.push(self.chroot_dir.clone());
            fs::create_dir_all(
                self.chroot_dir
                    .join(DATA_DRIVE_MOUNT_POINT.trim_start_matches('/')),
            )
            .await?;
            fs::create_dir_all(self.chroot_dir.join(JOURNAL_DIR.trim_start_matches('/'))).await?;
        }
        Ok(self)
    }

    pub async fn attach(self) -> Result<Self> {
        let vm = self.create().await?;
        if vm.is_container_running().await? {
            vm.stop_babel(false)?;
            vm.start_babel().await?;
        }
        Ok(vm)
    }

    fn stop_babel(&self, force: bool) -> Result<bool> {
        debug!("stop_babel for {}", self.vm_id);
        match get_process_pid(BABEL_BIN_NAME, &self.chroot_dir.to_string_lossy()) {
            Ok(pid) => {
                debug!("babel for {} has PID={}", self.vm_id, pid);
                if force {
                    kill_all_processes(
                        BABEL_BIN_NAME,
                        &[&self.chroot_dir.to_string_lossy()],
                        BABEL_KILL_TIMEOUT,
                        PosixSignal::SIGTERM,
                    );
                } else {
                    gracefully_terminate_process(pid, BABEL_KILL_TIMEOUT);
                }
                if is_process_running(pid) {
                    bail!("failed to stop babel for vm {}", self.vm_id);
                }
                Ok(true)
            }
            Err(GetProcessIdError::NotFound) => {
                debug!("babel for {} already stopped", self.vm_id);
                Ok(false)
            }
            Err(GetProcessIdError::MoreThanOne) => {
                let msg = format!(
                    "internal error, more than one babel process associated with one vm {}",
                    self.vm_id
                );
                error!(msg);
                bail!(msg);
            }
        }
    }

    async fn is_container_running(&self) -> Result<bool> {
        Ok(run_cmd(APPTAINER_BIN_NAME, ["instance", "list"])
            .await?
            .contains(&self.vm_name))
    }

    async fn stop_container(&self) -> Result<()> {
        if self.is_container_running().await? {
            run_cmd(APPTAINER_BIN_NAME, ["instance", "stop", &self.vm_name]).await?;
        }
        Ok(())
    }

    async fn start_container(&self) -> Result<()> {
        if !self.is_container_running().await? {
            let hostname = self.vm_name.replace('_', "-");
            let chroot_path = self.chroot_dir.to_string_lossy();
            let data_path = format!("{}:{}", self.data_dir.display(), DATA_DRIVE_MOUNT_POINT);
            let cpus = format!("{}", self.requirements.vcpu_count);
            let mem = format!("{}", self.requirements.mem_size_mb * 1_000_000);
            let net = format!("IP={}/{};GATEWAY={}", self.ip, self.mask_bits, self.gateway);
            let mut args = vec![
                "instance",
                "run",
                "--ipc",
                "--cleanenv",
                "--writable",
                "--no-mount",
                "home,cwd",
                "--bind",
                JOURNAL_DIR,
                "--bind",
                &data_path,
                "--hostname",
                &hostname,
            ];
            if self.config.cpu_limit {
                args.push("--cpus");
                args.push(&cpus);
            }
            if self.config.memory_limit {
                args.push("--memory");
                args.push(&mem);
            }
            if !self.config.host_network {
                args.append(&mut vec![
                    "--net",
                    "--network",
                    "bridge",
                    "--network-args",
                    &net,
                ]);
            }
            if let Some(extra_args) = self.config.extra_args.as_ref() {
                args.append(&mut extra_args.iter().map(|i| i.as_str()).collect());
            }
            args.push(&chroot_path);
            args.push(&self.vm_name);
            run_cmd(APPTAINER_BIN_NAME, args).await?;
        }
        Ok(())
    }

    async fn start_babel(&self) -> Result<()> {
        // start babel in chroot
        fs::copy(
            &self.babel_path,
            self.chroot_dir.join(BABEL_BIN_PATH.trim_start_matches('/')),
        )
        .await?;
        let mut cmd = Command::new(APPTAINER_BIN_NAME);
        cmd.args(["exec", "--ipc", "--cleanenv", "--userns", "--pid"]);
        cmd.args([
            &format!("instance://{}", self.vm_name),
            BABEL_BIN_NAME,
            &self.chroot_dir.to_string_lossy(),
        ]);
        debug!("start_babel for {}: '{:?}'", self.vm_id, cmd);
        cmd.spawn()?;
        let start = std::time::Instant::now();
        while let Err(GetProcessIdError::NotFound) =
            get_process_pid(BABEL_BIN_NAME, &self.chroot_dir.to_string_lossy())
        {
            if start.elapsed() < BABEL_START_TIMEOUT {
                sleep(Duration::from_millis(100)).await;
            } else {
                bail!("babel start timeout expired")
            }
        }
        Ok(())
    }

    async fn umount_all(&mut self) -> Result<()> {
        let chroot_dir = self.chroot_dir.to_string_lossy();
        let chroot_dir = chroot_dir.trim_end_matches('/');
        let mount_points = run_cmd("df", ["--all", "--output=target"])
            .await
            .map_err(|err| anyhow!("can't check if root fs is mounted, df: {err:#}"))?;
        let mut mount_points = mount_points
            .split_whitespace()
            .filter(|mount_point| mount_point.starts_with(chroot_dir))
            .collect::<Vec<_>>();
        mount_points.sort_by_key(|k| std::cmp::Reverse(k.len()));
        if !mount_points.is_empty() {
            let _ = run_cmd("fuser", ["-km", chroot_dir]).await;
            for mount_point in mount_points {
                with_retry!(
                    run_cmd("umount", [mount_point]),
                    UMOUNT_RETRY_MAX,
                    UMOUNT_BACKOFF_BASE_MS
                )
                .map_err(|err| anyhow!("failed to umount {mount_point}: {err:#}"))?;
            }
        }
        Ok(())
    }
}

async fn is_mounted(path: &Path) -> Result<bool> {
    let df_out = run_cmd("df", ["--all", "--output=target"])
        .await
        .map_err(|err| {
            anyhow!(
                "can't check if {} fs is mounted, df: {err:#}",
                path.display()
            )
        })?;
    Ok(df_out.contains(path.to_string_lossy().trim_end_matches('/')))
}

#[async_trait]
impl pal::VirtualMachine for ApptainerMachine {
    async fn state(&self) -> pal::VmState {
        match self.is_container_running().await {
            Ok(false) => pal::VmState::SHUTOFF,
            Ok(true) => match get_process_pid(BABEL_BIN_NAME, &self.chroot_dir.to_string_lossy()) {
                Ok(_) => pal::VmState::RUNNING,
                _ => pal::VmState::INVALID,
            },
            _ => pal::VmState::INVALID,
        }
    }

    async fn delete(&mut self) -> Result<()> {
        if self.shutdown().await.is_err() {
            self.force_shutdown().await?;
        }
        self.umount_all().await?;
        if self.node_dir.exists() {
            fs::remove_dir_all(&self.node_dir).await?;
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.stop_babel(false)?;
        self.stop_container().await?;
        Ok(())
    }

    async fn force_shutdown(&mut self) -> Result<()> {
        self.stop_babel(true)?;
        self.stop_container().await?;
        Ok(())
    }

    async fn start(&mut self) -> Result<()> {
        self.start_container().await?;
        self.start_babel().await
    }

    async fn release(&mut self) -> Result<()> {
        self.umount_all().await
    }

    async fn recover(&mut self) -> Result<()> {
        match self.is_container_running().await {
            Ok(false) => self.start().await,
            Ok(true) => match get_process_pid(BABEL_BIN_NAME, &self.chroot_dir.to_string_lossy()) {
                Ok(_) => Ok(()),
                Err(GetProcessIdError::NotFound) => self.start_babel().await,
                Err(GetProcessIdError::MoreThanOne) => {
                    kill_all_processes(
                        &self.babel_path.to_string_lossy(),
                        &[&self.chroot_dir.to_string_lossy()],
                        BABEL_KILL_TIMEOUT,
                        PosixSignal::SIGTERM,
                    );
                    self.start_babel().await
                }
            },
            _ => bail!("can't get container status for {}", self.vm_id),
        }
    }
}
