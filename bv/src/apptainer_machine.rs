use crate::{
    bv_config::ApptainerConfig,
    bv_context::BvContext,
    node_context, node_env,
    node_env::NODE_ENV_FILE_PATH,
    node_state::{NodeState, VmConfig},
    pal,
    utils::{get_process_pid, GetProcessIdError},
};
use async_trait::async_trait;
use babel_api::engine::{NodeEnv, PosixSignal};
use bv_utils::{
    cmd::run_cmd,
    system::{gracefully_terminate_process, is_process_running, kill_all_processes},
};
use eyre::{anyhow, bail, Context, Result};
use std::{
    ffi::OsStr,
    fmt::Debug,
    mem,
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};
use sysinfo::{Pid, PidExt};
use tokio::{fs, process::Command};
use tracing::warn;
use tracing::{debug, error};

pub const DATA_DIR: &str = "data";
pub const ROOTFS_DIR: &str = "rootfs";
pub const BACKUP_ROOTFS_DIR: &str = "rootfs_backup";
pub const PLUGIN_PATH: &str = "var/lib/babel/plugin";
pub const PLUGIN_MAIN_FILENAME: &str = "main.rhai";
const CGROUPS_CONF_FILE: &str = "cgroups.toml";
const APPTAINER_PID_FILE: &str = "apptainer.pid";
const JOURNAL_DIR: &str = "/run/systemd/journal";
const BABEL_BIN_NAME: &str = "babel";
const BABEL_KILL_TIMEOUT: Duration = Duration::from_secs(60);
const BABEL_BIN_PATH: &str = "/usr/bin/babel";
const APPTAINER_BIN_NAME: &str = "apptainer";
const BABEL_VAR_PATH: &str = "var/lib/babel";
const DATA_DRIVE_MOUNT_POINT: &str = "/blockjoy";
const PROTOCOL_DATA_PATH: &str = "/blockjoy/protocol_data";

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
    data_dir: PathBuf,

    apptainer_pid: Option<Pid>,
    babel_pid: Option<Pid>,

    vm_id: String,
    vm_name: String,
    ip: IpAddr,
    net_conf: NetConf,

    apptainer_config: ApptainerConfig,

    config: Config,
    backup_chroot_dir: PathBuf,
    config_backup: Option<Config>,
}

#[derive(Debug, Clone)]
pub struct NetConf {
    pub mask_bits: u8,
    pub gateway: IpAddr,
    pub bridge: IpAddr,
}

#[derive(Debug, Clone)]
struct Config {
    image_uri: String,
    vm: VmConfig,
    cpus: Vec<usize>,
    node_env: NodeEnv,
}

pub async fn new(
    bv_root: &Path,
    net_conf: NetConf,
    bv_context: &BvContext,
    node_state: &NodeState,
    babel_path: PathBuf,
    config: ApptainerConfig,
) -> Result<ApptainerMachine> {
    let node_dir = node_context::build_node_dir(bv_root, node_state.id);
    let chroot_dir = build_rootfs_dir(&node_dir);
    let backup_chroot_dir = node_dir.join(BACKUP_ROOTFS_DIR);
    let cgroups_path = node_dir.join(CGROUPS_CONF_FILE);
    let apptainer_pid_path = node_dir.join(APPTAINER_PID_FILE);
    let data_dir = node_dir.join(DATA_DIR);
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir).await?;
    }
    if node_state.initialized && babel_api::utils::protocol_data_stamp(&data_dir)?.is_none() {
        babel_api::utils::touch_protocol_data(&data_dir)?;
    }

    Ok(ApptainerMachine {
        node_dir,
        babel_path,
        chroot_dir,
        backup_chroot_dir,
        cgroups_path,
        apptainer_pid_path,
        apptainer_pid: None,
        babel_pid: None,
        data_dir,

        vm_id: node_state.id.to_string(),
        vm_name: node_state.name.clone(),
        ip: node_state.ip,
        net_conf,

        apptainer_config: config,

        config: Config {
            image_uri: node_state.image.uri.clone(),
            vm: node_state.vm_config.clone(),
            cpus: node_state.assigned_cpus.clone(),
            node_env: node_env::new(
                bv_context,
                node_state,
                PathBuf::from_str(DATA_DRIVE_MOUNT_POINT)?,
                PathBuf::from_str(PROTOCOL_DATA_PATH)?,
            ),
        },
        config_backup: None,
    })
}

impl ApptainerMachine {
    pub async fn build(&self) -> Result<()> {
        if !is_built(&self.chroot_dir).await? {
            run_cmd(
                "apptainer",
                [
                    OsStr::new("build"),
                    OsStr::new("--force"),
                    OsStr::new("--sandbox"),
                    &self.chroot_dir.clone().into_os_string(),
                    OsStr::new(&self.config.image_uri),
                ],
            )
            .await
            .map_err(|err| {
                anyhow!(
                    "failed to build '{}' from `{}`: {err:#}",
                    self.vm_id,
                    self.config.image_uri
                )
            })?;
            fs::create_dir_all(
                self.chroot_dir
                    .join(DATA_DRIVE_MOUNT_POINT.trim_start_matches('/')),
            )
            .await?;
            fs::create_dir_all(self.chroot_dir.join(JOURNAL_DIR.trim_start_matches('/'))).await?;
            fs::create_dir_all(self.chroot_dir.join(BABEL_VAR_PATH)).await?;
        }
        if self.apptainer_config.cpu_limit || self.apptainer_config.memory_limit {
            let mut content = String::new();
            if self.apptainer_config.memory_limit {
                content += &format!(
                    "memory.limit = {}\n",
                    self.config.vm.mem_size_mb * 1_000_000,
                )
            }
            if self.apptainer_config.cpu_limit && !self.config.cpus.is_empty() {
                content += &format!(
                    "cpu.cpus = \"{}\"\n",
                    self.config
                        .cpus
                        .iter()
                        .map(|cpu| cpu.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                )
            }
            fs::write(&self.cgroups_path, content).await?;
        }

        node_env::save(&self.config.node_env, &self.chroot_dir).await?;
        Ok(())
    }

    pub async fn attach(&mut self) -> Result<()> {
        self.build().await?;
        self.load_apptainer_pid().await?;
        if self.is_container_running().await {
            self.stop_babel(false).await?;
            self.start_babel().await?;
        }
        Ok(())
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
                    ["instance", "list", "--json", &self.vm_id],
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

    async fn is_container_running(&self) -> bool {
        self.apptainer_pid.map(is_process_running).unwrap_or(false)
    }

    async fn stop_container(&mut self) -> Result<()> {
        if self.is_container_running().await {
            if let Err(err) = run_cmd(APPTAINER_BIN_NAME, ["instance", "stop", &self.vm_id]).await {
                if run_cmd(APPTAINER_BIN_NAME, ["instance", "list", &self.vm_id])
                    .await?
                    .contains(&self.vm_id)
                {
                    return Err(err.into());
                } else {
                    warn!("container stop failed, but finally after all instance is gone: {err:#}");
                }
            }
        }
        self.apptainer_pid = None;
        if self.apptainer_pid_path.exists() {
            fs::remove_file(&self.apptainer_pid_path).await?;
        }
        Ok(())
    }

    async fn start_container(&mut self) -> Result<()> {
        if !self.is_container_running().await {
            let hostname = self.vm_name.replace('_', "-");
            let chroot_path = self.chroot_dir.to_string_lossy();
            let cgroups_path = self.cgroups_path.to_string_lossy();
            let apptainer_pid_path = self.apptainer_pid_path.to_string_lossy();
            let data_path = format!("{}:{}", self.data_dir.display(), DATA_DRIVE_MOUNT_POINT);
            let net = format!(
                "IP={}/{};GATEWAY={}",
                self.ip, self.net_conf.mask_bits, self.net_conf.bridge
            );
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
                "home,cwd,tmp",
            ];
            if self.apptainer_config.cpu_limit || self.apptainer_config.memory_limit {
                args.push("--apply-cgroups");
                args.push(&cgroups_path);
            }
            if !self.apptainer_config.host_network {
                args.append(&mut vec![
                    "--net",
                    "--network",
                    "bridge",
                    "--network-args",
                    &net,
                ]);
            }
            if let Some(extra_args) = self.apptainer_config.extra_args.as_ref() {
                args.append(&mut extra_args.iter().map(|i| i.as_str()).collect());
            }
            args.push(&chroot_path);
            args.push(&self.vm_id);
            run_cmd(APPTAINER_BIN_NAME, args).await?;
            self.load_apptainer_pid().await?;
            // force ARP table update
            if let Err(err) = run_cmd(
                APPTAINER_BIN_NAME,
                [
                    "exec",
                    &format!("instance://{}", self.vm_id),
                    "ping",
                    "-c",
                    "6",
                    "-i",
                    "0.25",
                    &self.net_conf.gateway.to_string(),
                ],
            )
            .await
            {
                warn!(
                    "ARP table update for node '{}' failed: {err:#}",
                    self.vm_name
                );
            }
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
        .await
        .with_context(|| format!("babel binary not found: {}", self.babel_path.display()))?;
        let mut cmd = Command::new(APPTAINER_BIN_NAME);
        cmd.args(["exec", "--ipc", "--cleanenv", "--userns", "--pid"]);
        cmd.args([
            OsStr::new("--env-file"),
            self.chroot_dir.join(NODE_ENV_FILE_PATH).as_os_str(),
        ]);
        cmd.args([
            &format!("instance://{}", self.vm_id),
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
}

async fn is_built(path: &Path) -> Result<bool> {
    if path.exists() {
        let labels_out: serde_json::Value = run_cmd(
            "apptainer",
            [
                OsStr::new("inspect"),
                OsStr::new("-d"),
                OsStr::new("--json"),
                path.as_os_str(),
            ],
        )
        .await
        .with_context(|| {
            format!(
                "can't check if {} vm is already built, `apptainer inspect -d`",
                path.display()
            )
        })
        .and_then(|out| {
            serde_json::from_str(&out).with_context(|| "invalid `apptainer inspect -d` output")
        })?;
        Ok(labels_out["data"]["attributes"]["deffile"].is_string())
    } else {
        Ok(false)
    }
}

#[async_trait]
impl pal::VirtualMachine for ApptainerMachine {
    async fn state(&self) -> pal::VmState {
        if self.is_container_running().await {
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

    async fn upgrade(&mut self, node_state: &NodeState) -> Result<()> {
        if self.is_container_running().await {
            bail!("can't upgrade running vm")
        }

        self.config_backup = Some(self.config.clone());
        self.config.image_uri = node_state.image.uri.clone();
        self.config.vm = node_state.vm_config.clone();
        self.config.cpus = node_state.assigned_cpus.clone();
        self.update_node_env(node_state);

        fs::rename(&self.chroot_dir, &self.backup_chroot_dir).await?;
        fs::create_dir_all(&self.chroot_dir).await?;

        self.build().await
    }

    async fn drop_backup(&mut self) -> Result<()> {
        if self.backup_chroot_dir.exists() {
            fs::remove_dir_all(&self.backup_chroot_dir).await?
        }
        self.config_backup = None;
        Ok(())
    }

    async fn rollback(&mut self) -> Result<()> {
        if self.backup_chroot_dir.exists() {
            fs::remove_dir_all(&self.chroot_dir).await?;
            fs::rename(&self.backup_chroot_dir, &self.chroot_dir).await?;
        }

        if let Some(mut backup) = self.config_backup.take() {
            mem::swap(&mut backup, &mut self.config);
            self.config_backup = Some(backup);
        }
        self.build().await?;

        Ok(())
    }

    async fn recover(&mut self) -> Result<()> {
        if self.is_container_running().await {
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

    fn node_env(&self) -> NodeEnv {
        self.config.node_env.clone()
    }

    fn update_node_env(&mut self, node_state: &NodeState) {
        node_env::update_state(&mut self.config.node_env, node_state);
    }

    fn plugin_path(&self) -> PathBuf {
        self.chroot_dir.join(PLUGIN_PATH).join(PLUGIN_MAIN_FILENAME)
    }

    fn data_dir(&self) -> PathBuf {
        self.node_dir.join(DATA_DIR)
    }
}
