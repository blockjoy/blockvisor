/// Default Platform Abstraction Layer implementation for Linux.
use crate::{
    nodes_manager::NodesDataCache,
    pal::{self, AvailableResources},
    BV_VAR_PATH,
};
use bv_utils::exp_backoff_timeout;
use eyre::{anyhow, bail, Context, Result};
use std::{fs, path::PathBuf, time::Instant};
use sysinfo::{DiskExt, System, SystemExt};

const ENV_BV_ROOT_KEY: &str = "BV_ROOT";

#[derive(Debug)]
pub struct LinuxPlatform {
    pub bv_root: PathBuf,
    pub babel_path: PathBuf,
    pub job_runner_path: PathBuf,
}

pub fn bv_root() -> PathBuf {
    PathBuf::from(std::env::var(ENV_BV_ROOT_KEY).unwrap_or_else(|_| "/".to_string()))
}

impl LinuxPlatform {
    pub async fn new() -> Result<Self> {
        let bv_root = bv_root();
        let babel_dir = fs::canonicalize(
            std::env::current_exe().with_context(|| "failed to get current binary path")?,
        )
        .with_context(|| "non canonical current binary path")?
        .parent()
        .ok_or_else(|| anyhow!("invalid current binary dir - has no parent"))?
        .join("../../babel/bin");
        let babel_path = babel_dir.join("babel");
        if !babel_path.exists() {
            bail!(
                "babel binary bundled with BV not found: {}",
                babel_path.display()
            )
        }
        let job_runner_path = babel_dir.join("babel_job_runner");
        if !job_runner_path.exists() {
            bail!(
                "job runner binary bundled with BV not found: {}",
                job_runner_path.display()
            )
        }
        Ok(Self {
            bv_root,
            babel_path,
            job_runner_path,
        })
    }

    pub fn available_resources(
        &self,
        nodes_data_cache: &NodesDataCache,
        used_disk_space_correction: u64,
    ) -> Result<AvailableResources> {
        let mut sys = System::new_all();
        sys.refresh_all();
        let (available_mem_size_mb, available_vcpu_count) = nodes_data_cache.iter().fold(
            (sys.total_memory() / 1_000_000, sys.cpus().len()),
            |(available_mem_size_mb, available_vcpu_count), (_, data)| {
                (
                    available_mem_size_mb - data.requirements.mem_size_mb,
                    available_vcpu_count - data.requirements.vcpu_count,
                )
            },
        );
        let available_disk_space =
            bv_utils::system::find_disk_by_path(&sys, &self.bv_root.join(BV_VAR_PATH))
                .map(|disk| disk.available_space())
                .ok_or_else(|| anyhow!("Cannot get available disk space"))?
                - used_disk_space_correction;
        Ok(AvailableResources {
            vcpu_count: available_vcpu_count,
            mem_size_mb: available_mem_size_mb,
            disk_size_gb: available_disk_space / 1_000_000_000,
        })
    }
}

const MAX_START_TRIES: u32 = 3;
const START_RECOVERY_BACKOFF_BASE_MS: u64 = 15_000;
const MAX_STOP_TRIES: u32 = 3;
const STOP_RECOVERY_BACKOFF_BASE_MS: u64 = 45_000;
const MAX_RECONNECT_TRIES: u32 = 3;
const RECONNECT_RECOVERY_BACKOFF_BASE_MS: u64 = 5_000;
const MAX_VM_TRIES: u32 = 3;
const VM_RECOVERY_BACKOFF_BASE_MS: u64 = 1_000;

#[derive(Debug, Default)]
pub struct RecoveryBackoff {
    reconnect: u32,
    stop: u32,
    start: u32,
    vm: u32,
    backoff_time: Option<Instant>,
}

impl pal::RecoverBackoff for RecoveryBackoff {
    fn backoff(&self) -> bool {
        if let Some(backoff_time) = &self.backoff_time {
            Instant::now() < *backoff_time
        } else {
            false
        }
    }

    fn reset(&mut self) {
        *self = Default::default();
    }

    fn start_failed(&mut self) -> bool {
        self.update_backoff_time(START_RECOVERY_BACKOFF_BASE_MS, self.start);
        self.start += 1;
        self.start >= MAX_START_TRIES
    }

    fn stop_failed(&mut self) -> bool {
        self.update_backoff_time(STOP_RECOVERY_BACKOFF_BASE_MS, self.stop);
        self.stop += 1;
        self.stop >= MAX_STOP_TRIES
    }

    fn reconnect_failed(&mut self) -> bool {
        self.update_backoff_time(RECONNECT_RECOVERY_BACKOFF_BASE_MS, self.reconnect);
        self.reconnect += 1;
        self.reconnect >= MAX_RECONNECT_TRIES
    }

    fn vm_recovery_failed(&mut self) -> bool {
        self.update_backoff_time(VM_RECOVERY_BACKOFF_BASE_MS, self.vm);
        self.vm += 1;
        self.vm >= MAX_VM_TRIES
    }
}

impl RecoveryBackoff {
    fn update_backoff_time(&mut self, backoff_base_ms: u64, counter: u32) {
        self.backoff_time = Some(Instant::now() + exp_backoff_timeout(backoff_base_ms, counter));
    }
}
