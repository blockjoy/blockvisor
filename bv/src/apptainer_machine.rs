use crate::{
    config::ApptainerConfig,
    node_context, node_env,
    node_env::NODE_ENV_FILE_PATH,
    node_state::NodeState,
    pal,
    services::blockchain,
    services::blockchain::ROOTFS_FILE,
    utils::{get_process_pid, GetProcessIdError},
};
use async_trait::async_trait;
use babel_api::engine::{NodeEnv, PosixSignal, DATA_DRIVE_MOUNT_POINT};
use bv_utils::{
    cmd::run_cmd,
    system::{gracefully_terminate_process, is_process_running, kill_all_processes},
    with_retry,
};
use eyre::{anyhow, bail, Result};
use std::{
    ffi::OsStr,
    fmt::Debug,
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};
use sysinfo::{Pid, PidExt};
use tokio::{fs, process::Command};
use tracing::log::warn;
use tracing::{debug, error};
use uuid::Uuid;

pub const DATA_DIR: &str = "data";
const ROOTFS_DIR: &str = "rootfs";
const CGROUPS_CONF_FILE: &str = "cgroups.toml";
const APPTAINER_PID_FILE: &str = "apptainer.pid";
const JOURNAL_DIR: &str = "/run/systemd/journal";
const BABEL_BIN_NAME: &str = "babel";
const BABEL_KILL_TIMEOUT: Duration = Duration::from_secs(60);
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
    cgroups_path: PathBuf,
    apptainer_pid_path: PathBuf,
    apptainer_pid: Option<Pid>,
    babel_pid: Option<Pid>,
    data_dir: PathBuf,
    os_img_path: PathBuf,
    vm_id: Uuid,
    vm_name: String,
    ip: IpAddr,
    mask_bits: u8,
    gateway: IpAddr,
    requirements: babel_api::metadata::Requirements,
    cpus: Vec<usize>,
    config: ApptainerConfig,
    node_env: NodeEnv,
}

pub async fn new(
    bv_root: &Path,
    gateway: IpAddr,
    mask_bits: u8,
    node_env: NodeEnv,
    node_state: &NodeState,
    babel_path: PathBuf,
    config: ApptainerConfig,
) -> Result<ApptainerMachine> {
    let node_dir = node_context::build_node_dir(bv_root, node_state.id);
    let chroot_dir = build_rootfs_dir(&node_dir);
    let cgroups_path = node_dir.join(CGROUPS_CONF_FILE);
    let apptainer_pid_path = node_dir.join(APPTAINER_PID_FILE);
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
        cgroups_path,
        apptainer_pid_path,
        apptainer_pid: None,
        babel_pid: None,
        data_dir,
        os_img_path,
        vm_id: node_state.id,
        vm_name: node_state.name.clone(),
        ip: node_state.network_interface.ip,
        mask_bits,
        gateway,
        requirements: node_state.requirements.clone(),
        cpus: node_state.assigned_cpus.clone(),
        config,
        node_env,
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
            fs::create_dir_all(self.chroot_dir.join(node_context::BABEL_VAR_PATH)).await?;
        }
        if self.config.cpu_limit || self.config.memory_limit {
            let mut content = String::new();
            if self.config.memory_limit {
                content += &format!(
                    "memory.limit = {}\n",
                    self.requirements.mem_size_mb * 1_000_000,
                )
            }
            if self.config.cpu_limit {
                content += &format!(
                    "cpu.cpus = \"{}\"\n",
                    self.cpus
                        .iter()
                        .map(|cpu| cpu.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                )
            }
            fs::write(&self.cgroups_path, content).await?;
        }

        node_env::save(&self.node_env, &self.chroot_dir).await?;
        Ok(self)
    }

    pub async fn attach(self) -> Result<Self> {
        let mut vm = self.create().await?;
        vm.load_apptainer_pid().await?;
        if vm.is_container_running() {
            vm.stop_babel(false).await?;
            vm.start_babel().await?;
        }
        Ok(vm)
    }

    async fn load_apptainer_pid(&mut self) -> Result<()> {
        if self.apptainer_pid_path.exists() {
            self.apptainer_pid = Some(Pid::from_str(
                fs::read_to_string(&self.apptainer_pid_path).await?.trim(),
            )?);
        } else {
            // fallback to `apptainer list`
            let json: serde_json::Value = serde_json::from_str(
                &run_cmd(
                    APPTAINER_BIN_NAME,
                    ["instance", "list", "--json", &self.vm_name],
                )
                .await?,
            )?;
            if let Some(pid) = json.get("instances").and_then(|instances| {
                instances.as_array().and_then(|instances| {
                    instances
                        .first()
                        .and_then(|instance| instance.get("pid").and_then(|pid| pid.as_u64()))
                })
            }) {
                self.apptainer_pid = Some(Pid::from_u32(pid as u32));
                if let Err(err) = fs::write(&self.apptainer_pid_path, format!("{pid}")).await {
                    warn!(
                        "failed to save container pid to file '{}': {:#}",
                        self.apptainer_pid_path.display(),
                        err
                    );
                }
            }
        }
        Ok(())
    }

    fn is_container_running(&self) -> bool {
        self.apptainer_pid.map(is_process_running).unwrap_or(false)
    }

    async fn stop_container(&mut self) -> Result<()> {
        if self.is_container_running() {
            run_cmd(APPTAINER_BIN_NAME, ["instance", "stop", &self.vm_name]).await?;
        }
        self.apptainer_pid = None;
        Ok(())
    }

    async fn start_container(&mut self) -> Result<()> {
        if !self.is_container_running() {
            let hostname = self.vm_name.replace('_', "-");
            let chroot_path = self.chroot_dir.to_string_lossy();
            let cgroups_path = self.cgroups_path.to_string_lossy();
            let apptainer_pid_path = self.apptainer_pid_path.to_string_lossy();
            let data_path = format!("{}:{}", self.data_dir.display(), DATA_DRIVE_MOUNT_POINT);
            let net = format!("IP={}/{};GATEWAY={}", self.ip, self.mask_bits, self.gateway);
            let mut args = vec![
                "instance",
                "run",
                "--containall",
                "--writable",
                "--pid-file",
                &apptainer_pid_path,
                "--bind",
                JOURNAL_DIR,
                "--bind",
                &data_path,
                "--hostname",
                &hostname,
                "--no-mount",
                "home,cwd",
            ];
            if self.config.cpu_limit || self.config.memory_limit {
                args.push("--apply-cgroups");
                args.push(&cgroups_path);
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
            self.load_apptainer_pid().await?;
        }
        Ok(())
    }

    fn is_babel_running(&self) -> bool {
        self.babel_pid.map(is_process_running).unwrap_or(false)
    }

    async fn stop_babel(&mut self, force: bool) -> Result<bool> {
        debug!("stop_babel for {}", self.vm_id);
        self.babel_pid = None;
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
                    gracefully_terminate_process(pid, BABEL_KILL_TIMEOUT).await;
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

    async fn start_babel(&mut self) -> Result<()> {
        // start babel in chroot
        fs::copy(
            &self.babel_path,
            self.chroot_dir.join(BABEL_BIN_PATH.trim_start_matches('/')),
        )
        .await?;
        let mut cmd = Command::new(APPTAINER_BIN_NAME);
        cmd.args(["exec", "--ipc", "--cleanenv", "--userns", "--pid"]);
        cmd.args([
            OsStr::new("--env-file"),
            self.chroot_dir.join(NODE_ENV_FILE_PATH).as_os_str(),
        ]);
        cmd.args([
            &format!("instance://{}", self.vm_name),
            BABEL_BIN_NAME,
            &self.chroot_dir.to_string_lossy(),
        ]);
        debug!("start_babel for {}: '{:?}'", self.vm_id, cmd);
        self.babel_pid = cmd.spawn()?.id().map(Pid::from_u32);
        if self.babel_pid.is_none() {
            bail!("failed to start babel for {}", self.vm_id)
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
        if self.is_container_running() {
            if self.is_babel_running() {
                pal::VmState::RUNNING
            } else {
                pal::VmState::INVALID
            }
        } else {
            pal::VmState::SHUTOFF
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
        self.stop_babel(false).await?;
        self.stop_container().await?;
        Ok(())
    }

    async fn force_shutdown(&mut self) -> Result<()> {
        self.stop_babel(true).await?;
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
        if self.is_container_running() {
            match get_process_pid(BABEL_BIN_NAME, &self.chroot_dir.to_string_lossy()) {
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
            }
        } else {
            self.start().await
        }
    }

    fn rootfs_dir(&self) -> &Path {
        &self.chroot_dir
    }
}
