use crate::bv_config::SharedConfig;
use crate::linux_platform::bv_root;
use crate::nodes_manager::NodesDataCache;
use crate::pal::Pal;
use crate::services::api::pb;
use crate::BV_VAR_PATH;
use crate::{api_with_retry, services};
use eyre::{anyhow, Context, Result};
use metrics::{register_gauge, Gauge};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use sysinfo::{CpuExt, DiskExt, NetworkExt, NetworksExt, System, SystemExt};
use systemstat::{saturating_sub_bytes, Platform, System as System2};

/// The interval by which we collect metrics from this host.
pub const COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

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
    pub memory_bytes: u64,
    pub disk_space_bytes: u64,
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
            memory_bytes: sys.total_memory(),
            disk_space_bytes: bv_utils::system::find_disk_by_path(
                &sys,
                &bv_root().canonicalize()?.join(BV_VAR_PATH),
            )
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

#[derive(Debug, Serialize, Deserialize)]
pub struct HostMetrics {
    pub used_cpu: u64,
    pub used_memory_bytes: u64,
    pub used_disk_space_bytes: u64,
    pub used_ips: Vec<String>,
    pub load_one: f64,
    pub load_five: f64,
    pub load_fifteen: f64,
    pub network_received_bytes: u64,
    pub network_sent_bytes: u64,
    pub uptime_secs: u64,
}

impl HostMetrics {
    pub fn set_all_gauges(&self) {
        SYSTEM_HOST_USED_CPU_GAUGE.set(self.used_cpu as f64);
        SYSTEM_HOST_USED_MEMORY_GAUGE.set(self.used_memory_bytes as f64);
        SYSTEM_HOST_USED_DISK_GAUGE.set(self.used_disk_space_bytes as f64);
        SYSTEM_HOST_LOAD_ONE_GAUGE.set(self.load_one);
        SYSTEM_HOST_LOAD_FIVE_GAUGE.set(self.load_five);
        SYSTEM_HOST_LOAD_FIFTEEN_GAUGE.set(self.load_fifteen);
        SYSTEM_HOST_NETWORK_RX_GAUGE.set(self.network_received_bytes as f64);
        SYSTEM_HOST_NETWORK_TX_GAUGE.set(self.network_sent_bytes as f64);
        SYSTEM_HOST_UPTIME_GAUGE.set(self.uptime_secs as f64);
    }

    pub async fn collect(nodes_data_cache: NodesDataCache, pal: &impl Pal) -> Result<Self> {
        let used_ips = nodes_data_cache
            .iter()
            .map(|(_, data)| data.ip.to_string())
            .collect();
        let disk_space_correction = pal
            .used_disk_space_correction(nodes_data_cache)
            .await
            .with_context(|| "failed to get used_disk_space_correction")?;

        let bv_root = bv_root();

        tokio::task::spawn_blocking(move || {
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
                used_cpu: sys.global_cpu_info().cpu_usage() as u64,
                used_memory_bytes: saturating_sub_bytes(mem.total, mem.free).as_u64(),
                used_disk_space_bytes: bv_utils::system::find_disk_by_path(
                    &sys,
                    &bv_root.canonicalize()?.join(BV_VAR_PATH),
                )
                .map(|disk| disk.total_space() - disk.available_space())
                .ok_or_else(|| anyhow!("Cannot get used disk space"))?
                    + disk_space_correction,
                used_ips,
                load_one: load.one,
                load_five: load.five,
                load_fifteen: load.fifteen,
                network_received_bytes: sys
                    .networks()
                    .iter()
                    .map(|(_, n)| n.total_received())
                    .sum(),
                network_sent_bytes: sys
                    .networks()
                    .iter()
                    .map(|(_, n)| n.total_transmitted())
                    .sum(),
                uptime_secs: sys.uptime(),
            })
        })
        .await?
    }
}

impl pb::MetricsServiceHostRequest {
    pub fn new(host_id: String, metrics: HostMetrics) -> Self {
        let metrics = pb::HostMetrics {
            used_cpu_hundreths: Some(metrics.used_cpu),
            used_memory_bytes: Some(metrics.used_memory_bytes),
            used_disk_bytes: Some(metrics.used_disk_space_bytes),
            load_one_percent: Some(metrics.load_one),
            load_five_percent: Some(metrics.load_five),
            load_fifteen_percent: Some(metrics.load_fifteen),
            network_received_bytes: Some(metrics.network_received_bytes),
            network_sent_bytes: Some(metrics.network_sent_bytes),
            used_ips: metrics.used_ips,
            uptime_seconds: Some(metrics.uptime_secs),
        };
        Self {
            metrics: HashMap::from([(host_id, metrics)]),
        }
    }
}

pub async fn send_info_update(config: SharedConfig) -> Result<()> {
    let info = HostInfo::collect()?;
    let mut client = services::ApiClient::build_with_default_connector(
        &config,
        pb::host_service_client::HostServiceClient::with_interceptor,
    )
    .await?;
    let update = pb::HostServiceUpdateRequest {
        host_id: config.read().await.id,
        name: Some(info.name),
        bv_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        os: Some(info.os),
        os_version: Some(info.os_version),
        region: None,
        disk_bytes: Some(info.disk_space_bytes),
        managed_by: None,
        update_tags: None,
    };
    api_with_retry!(client, client.update(update.clone()))?;

    Ok(())
}
