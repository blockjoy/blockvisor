use eyre::{anyhow, Result};
use std::cmp::Ordering;
use std::path::Path;
use sysinfo::{Disk, DiskExt, Pid, PidExt, ProcessExt, ProcessRefreshKind, System, SystemExt};

pub fn get_ip_address(ifa_name: &str) -> Result<String> {
    let ifas = local_ip_address::list_afinet_netifas()?;
    let (_, ip) = ifas
        .into_iter()
        .find(|(name, ipaddr)| name == ifa_name && ipaddr.is_ipv4())
        .ok_or_else(|| anyhow!("interface {ifa_name} not found"))?;
    Ok(ip.to_string())
}

pub fn is_process_running(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_process_specifics(Pid::from_u32(pid), ProcessRefreshKind::new())
        .then(|| sys.process(Pid::from_u32(pid)).map(|proc| proc.status()))
        .flatten()
        .map_or(false, |status| status != sysinfo::ProcessStatus::Zombie)
}

/// Find drive that depth of canonical mount point path is the biggest and at the same time
/// given `path` starts with it.
/// May return `None` if can't find such, but in worst case it should return `/` disk.
pub fn find_disk_by_path<'a>(sys: &'a System, path: &Path) -> Option<&'a Disk> {
    sys.disks()
        .iter()
        .max_by(|a, b| {
            match (
                a.mount_point().canonicalize(),
                b.mount_point().canonicalize(),
            ) {
                (Ok(a_mount_point), Ok(b_mount_point)) => {
                    match (
                        path.starts_with(&a_mount_point),
                        path.starts_with(&b_mount_point),
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
            if path.starts_with(mount_point) {
                Some(disk)
            } else {
                None
            }
        })
}

/// Get available disk space for drive on which given path reside.
pub fn available_disk_space_by_path(path: &Path) -> Result<u64> {
    let mut sys = System::new_all();
    sys.refresh_all();
    find_disk_by_path(&sys, path)
        .map(|disk| disk.available_space())
        .ok_or_else(|| anyhow!("Cannot get available disk space"))
}
