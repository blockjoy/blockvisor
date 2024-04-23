use crate::config::ApptainerConfig;
use crate::services::blockchain;
use crate::services::blockchain::ROOT_FS_FILE;
use crate::utils::{get_process_pid, GetProcessIdError};
use crate::{node_data::NodeData, pal, BV_VAR_PATH};
use async_trait::async_trait;
use babel_api::engine::{PosixSignal, DATA_DRIVE_MOUNT_POINT};
use bv_utils::cmd::run_cmd;
use bv_utils::system::{gracefully_terminate_process, is_process_running, kill_all_processes};
use bv_utils::with_retry;
use eyre::{anyhow, bail, Result};
use std::ffi::OsStr;
use std::fmt::Debug;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, error};
use uuid::Uuid;

const BARE_NODES_DIR: &str = "bare";
pub const CHROOT_DIR: &str = "os";
const DATA_DIR: &str = "data";
const JOURNAL_DIR: &str = "/run/systemd/journal";
pub const BABEL_BIN_NAME: &str = "babel";
const BABEL_KILL_TIMEOUT: Duration = Duration::from_secs(60);
const BABEL_START_TIMEOUT: Duration = Duration::from_secs(3);
const UMOUNT_RETRY_MAX: u32 = 2;
const UMOUNT_BACKOFF_BASE_MS: u64 = 1000;
const BABEL_BIN_PATH: &str = "/usr/bin/babel";
const APPTAINER_BIN_NAME: &str = "apptainer";

pub fn build_vm_data_path(bv_root: &Path, id: Uuid) -> PathBuf {
    bv_root
        .join(BV_VAR_PATH)
        .join(BARE_NODES_DIR)
        .join(id.to_string())
}

#[derive(Debug)]
pub struct ApptainerMachine {
    vm_dir: PathBuf,
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
    node_data: &NodeData<impl pal::NetInterface>,
    babel_path: PathBuf,
    config: ApptainerConfig,
) -> Result<ApptainerMachine> {
    let vm_dir = build_vm_data_path(bv_root, node_data.id);
    let chroot_dir = vm_dir.join(CHROOT_DIR);
    fs::create_dir_all(&chroot_dir).await?;
    let data_dir = vm_dir.join(DATA_DIR);
    fs::create_dir_all(&data_dir).await?;
    let os_img_path = vm_dir.join(ROOT_FS_FILE);
    if !os_img_path.exists() {
        fs::copy(
            blockchain::get_image_download_folder_path(bv_root, &node_data.image)
                .join(ROOT_FS_FILE),
            &os_img_path,
        )
        .await?;
    }
    let ip = *node_data.network_interface.ip();
    Ok(ApptainerMachine {
        vm_dir,
        babel_path,
        chroot_dir,
        data_dir,
        os_img_path,
        vm_id: node_data.id,
        vm_name: node_data.name.clone(),
        ip,
        mask_bits,
        gateway,
        requirements: node_data.requirements.clone(),
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
            mount([
                self.os_img_path.clone().into_os_string(),
                self.chroot_dir.clone().into_os_string(),
            ])
            .await?;
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
            .contains(&format!("{}", self.vm_id)))
    }

    async fn stop_container(&self) -> Result<()> {
        if self.is_container_running().await? {
            run_cmd(
                APPTAINER_BIN_NAME,
                ["instance", "stop", &format!("{}", self.vm_id)],
            )
            .await?;
        }
        Ok(())
    }

    async fn start_container(&self) -> Result<()> {
        if !self.is_container_running().await? {
            let mut args = vec![
                "instance".to_string(),
                "run".to_string(),
                "--writable".to_string(),
                "--bind".to_string(),
                JOURNAL_DIR.to_owned(),
                "--bind".to_string(),
                format!("{}:{}", self.data_dir.display(), DATA_DRIVE_MOUNT_POINT),
                "--hostname".to_string(),
                self.vm_name.replace('_', "-"),
            ];
            if self.config.cpu_limit {
                args.push("--cpus".to_string());
                args.push(format!("{}", self.requirements.vcpu_count));
            }
            if self.config.memory_limit {
                args.push("--memory".to_string());
                args.push(format!("{}", self.requirements.mem_size_mb * 1_000_000));
            }
            if !self.config.host_network {
                args.append(&mut vec![
                    "--net".to_string(),
                    "--network".to_string(),
                    "bridge".to_string(),
                    "--network-args".to_string(),
                    format!("IP={}/{};GATEWAY={}", self.ip, self.mask_bits, self.gateway),
                ]);
            }
            if let Some(extra_args) = self.config.extra_args.as_ref() {
                args.append(&mut extra_args.clone());
            }
            args.push(self.chroot_dir.to_string_lossy().to_string());
            args.push(format!("{}", self.vm_id));
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
        cmd.args([
            "exec",
            &format!("instance://{}", self.vm_id),
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

async fn mount<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S> + Debug,
    S: AsRef<OsStr>,
{
    let cmd = format!("mount {args:?}");
    run_cmd("mount", args)
        .await
        .map_err(|err| anyhow!("failed to '{cmd}': {err:#}"))?;
    Ok(())
}

#[async_trait]
impl pal::VirtualMachine for ApptainerMachine {
    async fn state(&self) -> pal::VmState {
        match self.is_container_running().await {
            Ok(true) => pal::VmState::RUNNING,
            _ => pal::VmState::SHUTOFF,
        }
    }

    async fn delete(&mut self) -> Result<()> {
        if self.shutdown().await.is_err() {
            self.force_shutdown().await?;
        }
        self.umount_all().await?;
        if self.vm_dir.exists() {
            fs::remove_dir_all(&self.vm_dir).await?;
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

    async fn detach(&mut self) -> Result<()> {
        self.umount_all().await
    }
}
