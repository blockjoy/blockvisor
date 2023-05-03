use crate::services::api::pb;
use anyhow::{anyhow, Result};
use metrics::{register_gauge, Gauge};
use std::collections::HashMap;
use sysinfo::{CpuExt, DiskExt, NetworkExt, NetworksExt, System, SystemExt};
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

        let disk_size = sys
            .disks()
            .iter()
            .filter(|disk| disk.name() != "overlay")
            .fold(0, |acc, disk| acc + disk.total_space());

        let info = Self {
            name: sys
                .host_name()
                .ok_or_else(|| anyhow!("Cannot get host name"))?,
            cpu_count: sys.cpus().len(),
            mem_size: sys.total_memory(),
            disk_size,
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
            used_disk_space: sys
                .disks()
                .iter()
                // TODO: this includes almost all drives, we need to figure out which drives we should count
                // here and which ones we need to skip. Loopback devices should probably be skipped for
                // example.
                .filter(|disk| disk.name() != "overlay")
                .map(|d| d.total_space() - d.available_space())
                .sum(),
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
