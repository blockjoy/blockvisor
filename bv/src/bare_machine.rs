use crate::services::blockchain;
use crate::services::blockchain::ROOT_FS_FILE;
use crate::utils::{get_process_pid, GetProcessIdError};
use crate::{node_data::NodeData, pal, BV_VAR_PATH};
use async_trait::async_trait;
use babel_api::engine::PosixSignal;
use bv_utils::cmd::run_cmd;
use bv_utils::system::{gracefully_terminate_process, is_process_running, kill_all_processes};
use eyre::{anyhow, bail, Result};
use std::ffi::OsStr;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, error};
use uuid::Uuid;

const BARE_NODES_DIR: &str = "bare";
pub const CHROOT_DIR: &str = "os";
pub const BABEL_BIN_NAME: &str = "babel";
const BABEL_KILL_TIMEOUT: Duration = Duration::from_secs(60);

pub fn build_vm_data_path(bv_root: &Path, id: Uuid) -> PathBuf {
    bv_root
        .join(BV_VAR_PATH)
        .join(BARE_NODES_DIR)
        .join(id.to_string())
}

#[derive(Debug)]
pub struct BareMachine {
    bv_root: PathBuf,
    vm_dir: PathBuf,
    babel_path: PathBuf,
    chroot_dev_dir: PathBuf,
    chroot_proc_dir: PathBuf,
    chroot_dir: PathBuf,
    os_img_path: PathBuf,
    vm_id: Uuid,
}

pub async fn new(
    bv_root: &Path,
    node_data: &NodeData<impl pal::NetInterface>,
    babel_path: PathBuf,
) -> Result<BareMachine> {
    let vm_dir = build_vm_data_path(bv_root, node_data.id);
    let chroot_dir = vm_dir.join(CHROOT_DIR);
    fs::create_dir_all(&chroot_dir).await?;
    let os_img_path = vm_dir.join(ROOT_FS_FILE);
    if !os_img_path.exists() {
        fs::copy(
            blockchain::get_image_download_folder_path(bv_root, &node_data.image)
                .join(ROOT_FS_FILE),
            &os_img_path,
        )
        .await?;
    }
    Ok(BareMachine {
        bv_root: bv_root.to_path_buf(),
        vm_dir,
        babel_path,
        chroot_proc_dir: chroot_dir.join("proc"),
        chroot_dev_dir: chroot_dir.join("dev"),
        chroot_dir,
        os_img_path,
        vm_id: node_data.id,
    })
}

impl BareMachine {
    pub async fn create(self) -> Result<Self> {
        self.attach().await
    }

    pub async fn attach(self) -> Result<Self> {
        if !is_mounted(&self.chroot_dir).await? {
            mount([
                self.os_img_path.clone().into_os_string(),
                self.chroot_dir.clone().into_os_string(),
            ])
            .await?;
        }
        if !is_mounted(&self.chroot_proc_dir).await? {
            mount([
                OsStr::new("-t"),
                OsStr::new("proc"),
                OsStr::new("proc"),
                &self.chroot_proc_dir.clone().into_os_string(),
            ])
            .await?;
        }
        if !is_mounted(&self.chroot_dev_dir).await? {
            mount([
                OsStr::new("-o"),
                OsStr::new("bind"),
                &self.bv_root.join("dev").into_os_string(),
                &self.chroot_dev_dir.clone().into_os_string(),
            ])
            .await?;
        }
        if self.stop_babel(false)? {
            self.start_babel()?;
        }
        Ok(self)
    }

    fn stop_babel(&self, force: bool) -> Result<bool> {
        debug!("stop_babel for {}", self.vm_id);
        match get_process_pid(BABEL_BIN_NAME, &self.chroot_dir.to_string_lossy()) {
            Ok(pid) => {
                debug!("babel for {} has PID={}", self.vm_id, pid);
                if force {
                    kill_all_processes(
                        &self.babel_path.to_string_lossy(),
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

    fn start_babel(&self) -> Result<()> {
        debug!("start_babel for {}", self.vm_id);
        // start babel in chroot
        let mut cmd = Command::new(&self.babel_path);
        cmd.arg(&self.chroot_dir);
        cmd.spawn()?;
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
impl pal::VirtualMachine for BareMachine {
    fn state(&self) -> pal::VmState {
        match get_process_pid(BABEL_BIN_NAME, &self.chroot_dir.to_string_lossy()) {
            Ok(_) => pal::VmState::RUNNING,
            _ => pal::VmState::SHUTOFF,
        }
    }

    async fn delete(&mut self) -> Result<()> {
        if self.shutdown().await.is_err() {
            self.force_shutdown().await?;
        }
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
                run_cmd("umount", [mount_point])
                    .await
                    .map_err(|err| anyhow!("failed to umount {mount_point}: {err:#}"))?;
            }
        }
        if self.vm_dir.exists() {
            fs::remove_dir_all(&self.vm_dir).await?;
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.stop_babel(false)?;
        Ok(())
    }

    async fn force_shutdown(&mut self) -> Result<()> {
        self.stop_babel(true)?;
        Ok(())
    }

    async fn start(&mut self) -> Result<()> {
        self.start_babel()
    }
}
