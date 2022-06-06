use crate::containers::{ContainerStatus, Containers, DummyNode, NodeContainer};
use anyhow::Result;
use sysinfo::{DiskExt, System, SystemExt};
use tracing::info;

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
    let sys = System::new_all();

    HostInfo {
        name: sys.host_name(),
        cpu_count: sys.physical_core_count().map(|x| x as i64),
        mem_size: Some(sys.total_memory() as i64 * 1024),
        disk_size: Some(sys.disks()[0].total_space() as i64), // todo: display either for all disks or install partition
        os: sys.name(),
        os_version: sys.os_version(),
    }
}

pub fn get_ip_address(ifa_name: &str) -> String {
    let ifas = local_ip_address::list_afinet_netifas().unwrap();
    let (_, ip) = local_ip_address::find_ifa(ifas, ifa_name).unwrap();
    ip.to_string()
}

// used for testing purposes
pub async fn dummy_apply_config(containers: &mut Containers) -> Result<()> {
    for (id, container_config) in &containers.containers {
        let id = *id;
        // remove deleted nodes
        if container_config.status == ContainerStatus::Deleted {
            if DummyNode::exists(id).await {
                let mut node = DummyNode::connect(id, &containers.network_interface()).await?;
                node.delete().await?;
            }
        } else {
            // create non existing nodes
            if !DummyNode::exists(id).await {
                DummyNode::create(id, &containers.network_interface()).await?;
                containers.machine_index += 1;
            }

            // fix nodes status
            let mut node = DummyNode::connect(id, &containers.network_interface()).await?;
            let state = node.state().await?;
            if state != container_config.status {
                info!(
                    "Changing state from {:?} to {:?}: {}",
                    state, container_config.status, id
                );
                match container_config.status {
                    ContainerStatus::Started => node.start().await?,
                    ContainerStatus::Stopped => node.kill().await?,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
