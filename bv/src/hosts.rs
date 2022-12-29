use crate::services::api::pb;
use anyhow::Result;
use std::collections::HashMap;
use sysinfo::{CpuExt, DiskExt, NetworkExt, NetworksExt, System, SystemExt};
use systemstat::{saturating_sub_bytes, Platform, System as System2};

/// The interval by which we collect metrics from this host.
pub const COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
pub struct HostInfo {
    pub name: Option<String>,
    pub cpu_count: Option<i64>, // because postgres does not have unsigned
    pub mem_size: Option<i64>,
    pub disk_size: Option<i64>,
    pub os: Option<String>,
    pub os_version: Option<String>,
}

pub fn get_host_info() -> HostInfo {
    let mut sys = System::new_all();
    sys.refresh_all();

    let disk_size = sys
        .disks()
        .iter()
        .fold(0, |acc, disk| acc + disk.total_space());

    HostInfo {
        name: sys.host_name(),
        cpu_count: sys.physical_core_count().map(|x| x as i64),
        mem_size: Some(sys.total_memory() as i64),
        disk_size: Some(disk_size as i64),
        os: sys.name(),
        os_version: sys.os_version(),
    }
}

pub fn get_ip_address(ifa_name: &str) -> String {
    let ifas = local_ip_address::list_afinet_netifas().unwrap();
    let (_, ip) = local_ip_address::find_ifa(ifas, ifa_name).unwrap();
    ip.to_string()
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

pub fn get_host_metrics() -> Result<HostMetrics> {
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
            // TODO: this includes all drives, we need to figure out which drives we should count
            // here and which ones we need to skip. Loopback devices should probably be skipped for
            // example.
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

impl pb::HostMetricsRequest {
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
