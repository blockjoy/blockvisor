use std::cmp::Ordering;
use std::path::Path;
use sysinfo::{Disk, DiskExt, System, SystemExt};

/// Find drive that depth of canonical mount point path is the biggest and at the same time
/// given `path` starts with it.
/// May return `None` if can't find such, but in worst case it should return `/` disk.
pub fn find_disk_by_path<'a, 'b>(sys: &'a System, path: &'b Path) -> Option<&'a Disk> {
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
        assert!(
            find_disk_by_path(&sys, &bv_root().canonicalize().unwrap().join(BV_VAR_PATH)).is_some()
        );
    }
}
