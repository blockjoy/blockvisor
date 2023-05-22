use crate::linux_platform::bv_root;
use crate::services::api::pb;
use crate::BV_VAR_PATH;
use anyhow::{anyhow, Result};
use metrics::{register_gauge, Gauge};
use std::cmp::Ordering;
use std::collections::HashMap;
use sysinfo::{CpuExt, Disk, DiskExt, NetworkExt, NetworksExt, System, SystemExt};
use systemstat::{saturating_sub_bytes, Platform, System as System2};

/// The interval by which we collect metrics from this host.
pub const COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

lazy_static::lazy_static! {
    pub static ref SYSTEM_HOST_USED_CPU_GAUGE: Gauge = register_gauge!("system.host.used_cpu");
    pub static ref SYSTEM_HOST_USED_MEMORY_GAUGE: Gauge = register_gauge!("system.host.used_memory");
    pub static ref SYSTEM_HOST_USED_DISK_GAUGE: Gauge = register_gauge!("system.host.used_disk_space");
    pub static ref SYSTEM_HOST_LOAD_ONE_GAUGE: Gauge = register_gauge!("system.host.load_one");
    pub static ref SYSTEM_HOST_LOAD_FIVE_GAUGE: Gauge = register_gauge!("system.host.load_five");
    pub static ref SYSTEM_HOST_LOAD_FIFTEEN_GAUGE: Gauge = register_gauge!("system.host.load_fifteen");
    pub static ref SYSTEM_HOST_NETWORK_RX_GAUGE: Gauge = register_gauge!("system.host.network_received");
    pub static ref SYSTEM_HOST_NETWORK_TX_GAUGE: Gauge = register_gauge!("system.host.network_sent");
    pub static ref SYSTEM_HOST_UPTIME_GAUGE: Gauge = register_gauge!("system.host.uptime");
}

#[derive(Debug)]
pub struct HostInfo {
    pub name: String,
    pub cpu_count: usize,
    pub mem_size: u64,
    pub disk_size: u64,
    pub os: String,
    pub os_version: String,
}

impl HostInfo {
    pub fn collect() -> Result<Self> {
        let mut sys = System::new_all();
        sys.refresh_all();

        let info = Self {
            name: sys
                .host_name()
                .ok_or_else(|| anyhow!("Cannot get host name"))?,
            cpu_count: sys.cpus().len(),
            mem_size: sys.total_memory(),
            disk_size: find_bv_var_disk(&sys)
                .map(|disk| disk.total_space())
                .ok_or_else(|| anyhow!("Cannot get disk size"))?,
            os: sys.name().ok_or_else(|| anyhow!("Cannot get OS name"))?,
            os_version: sys
                .os_version()
                .ok_or_else(|| anyhow!("Cannot get OS version"))?,
        };

        Ok(info)
    }
}

#[derive(Debug)]
pub struct HostMetrics {
    pub used_cpu: u32,
    pub used_memory: u64,
    pub used_disk_space: u64,
    pub load_one: f64,
    pub load_five: f64,
    pub load_fifteen: f64,
    pub network_received: u64,
    pub network_sent: u64,
    pub uptime: u64,
}

impl HostMetrics {
    pub fn set_all_gauges(&self) {
        SYSTEM_HOST_USED_CPU_GAUGE.set(self.used_cpu as f64);
        SYSTEM_HOST_USED_MEMORY_GAUGE.set(self.used_memory as f64);
        SYSTEM_HOST_USED_DISK_GAUGE.set(self.used_disk_space as f64);
        SYSTEM_HOST_LOAD_ONE_GAUGE.set(self.load_one);
        SYSTEM_HOST_LOAD_FIVE_GAUGE.set(self.load_five);
        SYSTEM_HOST_LOAD_FIFTEEN_GAUGE.set(self.load_fifteen);
        SYSTEM_HOST_NETWORK_RX_GAUGE.set(self.network_received as f64);
        SYSTEM_HOST_NETWORK_TX_GAUGE.set(self.network_sent as f64);
        SYSTEM_HOST_UPTIME_GAUGE.set(self.uptime as f64);
    }

    pub fn collect() -> Result<Self> {
        let mut sys = System::new_all();
        // We need to refresh twice:
        // https://docs.rs/sysinfo/latest/sysinfo/trait.CpuExt.html#tymethod.cpu_usage
        sys.refresh_all();
        sys.refresh_cpu_specifics(sysinfo::CpuRefreshKind::new().with_cpu_usage());

        // sysinfo produced wrong results, so let's try how this crate works
        let sys2 = System2::new();
        let mem = sys2.memory()?;

        let load = sys.load_average();
        Ok(HostMetrics {
            used_cpu: sys.global_cpu_info().cpu_usage() as u32,
            used_memory: saturating_sub_bytes(mem.total, mem.free).as_u64(),
            used_disk_space: find_bv_var_disk(&sys)
                .map(|disk| disk.total_space() - disk.available_space())
                .ok_or_else(|| anyhow!("Cannot get used disk space"))?,
            load_one: load.one,
            load_five: load.five,
            load_fifteen: load.fifteen,
            network_received: sys.networks().iter().map(|(_, n)| n.total_received()).sum(),
            network_sent: sys
                .networks()
                .iter()
                .map(|(_, n)| n.total_transmitted())
                .sum(),
            uptime: sys.uptime(),
        })
    }
}

impl pb::MetricsServiceHostRequest {
    pub fn new(host_id: String, metrics: HostMetrics) -> Self {
        let metrics = pb::HostMetrics {
            used_cpu: Some(metrics.used_cpu),
            used_memory: Some(metrics.used_memory),
            used_disk_space: Some(metrics.used_disk_space),
            load_one: Some(metrics.load_one),
            load_five: Some(metrics.load_five),
            load_fifteen: Some(metrics.load_fifteen),
            network_received: Some(metrics.network_received),
            network_sent: Some(metrics.network_sent),
            uptime: Some(metrics.uptime),
        };
        Self {
            metrics: HashMap::from([(host_id, metrics)]),
        }
    }
}

/// Find drive that depth of canonical mount point path is the biggest and at the same time
/// `<root>/BV_VAR_PATH` starts with it.
/// May return `None` if can't find such, but in worst case it should return `/` disk.
fn find_bv_var_disk(sys: &System) -> Option<&Disk> {
    let bv_var_path = bv_root().canonicalize().ok()?.join(BV_VAR_PATH);
    sys.disks()
        .iter()
        .max_by(|a, b| {
            match (
                a.mount_point().canonicalize(),
                b.mount_point().canonicalize(),
            ) {
                (Ok(a_mount_point), Ok(b_mount_point)) => {
                    match (
                        bv_var_path.starts_with(&a_mount_point),
                        bv_var_path.starts_with(&b_mount_point),
                    ) {
                        (true, true) => a_mount_point
                            .ancestors()
                            .count()
                            .cmp(&b_mount_point.ancestors().count()),
                        (false, true) => Ordering::Less,
                        (true, false) => Ordering::Greater,
                        (false, false) => Ordering::Equal,
                    }
                }
                (Err(_), Ok(_)) => Ordering::Less,
                (Ok(_), Err(_)) => Ordering::Greater,
                (Err(_), Err(_)) => Ordering::Equal,
            }
        })
        .and_then(|disk| {
            let mount_point = disk.mount_point().canonicalize().ok()?;
            if bv_var_path.starts_with(mount_point) {
                Some(disk)
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_info_collect() {
        assert!(HostInfo::collect().is_ok());
    }

    #[test]
    fn test_host_metrics_collect() {
        assert!(HostMetrics::collect().is_ok());
    }

    #[test]
    fn test_find_bv_var_disk() {
        let mut sys = System::new_all();
        sys.refresh_all();
        // Theoretically it may return `None` in some edge cases, but normally it should not happen
        // `/` disk should be returned in worst case.
        assert!(find_bv_var_disk(&sys).is_some());
    }
}
