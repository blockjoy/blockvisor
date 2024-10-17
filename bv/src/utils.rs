use crate::workspace;
use bv_utils::{cmd::run_cmd, with_retry};
use cidr_utils::cidr::Ipv4Cidr;
use eyre::{anyhow, bail, Context, Result};
use rand::Rng;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    ffi::OsStr,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};
use sysinfo::{Pid, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use thiserror::Error;
use tokio::{
    fs::{self},
    io::AsyncWriteExt,
};
use tracing::debug;

// image download should never take more than 15min
const ARCHIVE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Error, Debug)]
pub enum GetProcessIdError {
    #[error("process not found")]
    NotFound,
    #[error("found more than 1 matching process")]
    MoreThanOne,
}

/// Get the pid of the running VM process knowing its process name and part of command line.
pub fn get_process_pid(process_name: &str, cmd: &str) -> Result<Pid, GetProcessIdError> {
    let mut sys = System::new();
    debug!("Retrieving pid for process `{process_name}` and cmd like `{cmd}`");
    sys.refresh_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    let processes: Vec<_> = sys
        .processes_by_name(process_name)
        .filter(|&process| {
            process.cmd().contains(&cmd.to_string())
                && process.status() != sysinfo::ProcessStatus::Zombie
        })
        .collect();

    match processes.len() {
        0 => Err(GetProcessIdError::NotFound),
        1 => Ok(processes[0].pid()),
        _ => Err(GetProcessIdError::MoreThanOne),
    }
}

/// Get pids of the running VM processes.
pub fn get_all_processes_pids(process_name: &str) -> Result<Vec<Pid>> {
    let mut sys = System::new();
    debug!("Retrieving pids for processes of `{process_name}`");
    sys.refresh_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    Ok(sys
        .processes_by_name(process_name)
        .filter(|&process| {
            process.status() != sysinfo::ProcessStatus::Zombie && process.name() == process_name
        })
        .map(|process| process.pid())
        .collect())
}

pub struct Archive(PathBuf);
impl Archive {
    pub async fn ungzip(self) -> Result<Self> {
        // pigz is parallel and fast
        // TODO: pigz is external dependency, we need a reliable way of delivering it to hosts
        run_cmd(
            "pigz",
            [
                OsStr::new("--decompress"),
                OsStr::new("--force"),
                self.0.as_os_str(),
            ],
        )
        .await?;
        if let (Some(parent), Some(name)) = (self.0.parent(), self.0.file_stem()) {
            Ok(Self(parent.join(name)))
        } else {
            bail!("invalid gzip file path {}", self.0.to_string_lossy())
        }
    }

    pub async fn untar(self) -> Result<Self> {
        let Some(parent_dir) = self.0.parent() else {
            bail!("invalid tar file path {}", self.0.to_string_lossy())
        };
        run_cmd(
            "tar",
            [
                OsStr::new("-C"),
                parent_dir.as_os_str(),
                OsStr::new("-xf"),
                self.0.as_os_str(),
            ],
        )
        .await?;
        let _ = fs::remove_file(&self.0).await;

        Ok(Self(parent_dir.into()))
    }
}

const DOWNLOAD_RETRY_MAX: u32 = 3;
const DOWNLOAD_BACKOFF_BASE_SEC: u64 = 3;

pub async fn download_archive_with_retry(url: &str, path: PathBuf) -> Result<Archive> {
    with_retry!(
        download_file(url, &path),
        DOWNLOAD_RETRY_MAX,
        DOWNLOAD_BACKOFF_BASE_SEC
    )
    .with_context(|| format!("download failed for {url}"))?;
    Ok(Archive(path))
}

pub async fn download_archive(url: &str, path: PathBuf) -> Result<Archive> {
    download_file(url, &path).await?;
    Ok(Archive(path))
}

async fn download_file(url: &str, path: &PathBuf) -> Result<()> {
    debug!("Downloading url...");
    let _ = fs::remove_file(path).await;
    let mut file = fs::File::create(path).await?;

    let client = reqwest::Client::builder()
        .timeout(ARCHIVE_DOWNLOAD_TIMEOUT)
        .build()?;
    let mut resp = client.get(url).send().await?;

    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
    }

    file.flush().await?;
    debug!("Done downloading");

    Ok(())
}

pub fn semver_cmp(a: &str, b: &str) -> Ordering {
    match (Version::parse(a), Version::parse(b)) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        (Ok(_), Err(_)) => Ordering::Greater,
        (Err(_), Ok(_)) => Ordering::Less,
        (Err(_), Err(_)) => Ordering::Equal,
    }
}

/// Take base interval and add random amount of seconds to it
///
/// Do not add more than original seconds / 2
pub fn with_jitter(base: Duration) -> Duration {
    let mut rng = rand::thread_rng();
    let jitter_max = base.as_secs() / 2;
    let jitter = Duration::from_secs(rng.gen_range(0..jitter_max));
    base + jitter
}

/// Struct to capture output of linux `ip --json route` command
#[derive(Deserialize, Serialize, Debug)]
pub struct IpRoute {
    pub dst: String,
    pub gateway: Option<String>,
    pub dev: String,
    pub prefsrc: Option<String>,
    pub protocol: Option<String>,
}

#[derive(Default, Debug, PartialEq)]
pub struct NetParams {
    pub ip: Option<String>,
    pub gateway: Option<String>,
    pub ip_from: Option<String>,
    pub ip_to: Option<String>,
}

pub fn next_available_ip(net_params: &NetParams, used: &[String]) -> Result<String> {
    let range = ipnet::Ipv4AddrRange::new(
        net_params
            .ip_from
            .as_ref()
            .ok_or(anyhow!("missing ip_from"))?
            .parse()?,
        net_params
            .ip_to
            .as_ref()
            .ok_or(anyhow!("missing ip_to"))?
            .parse()?,
    );
    range
        .into_iter()
        .find(|ip| !used.contains(&ip.to_string()))
        .map(|ip| ip.to_string())
        .ok_or(anyhow!("no available ip in range {:?}", range))
}

pub fn is_dev_ip(route: &IpRoute, dev: &str) -> bool {
    route.dev == dev
        && route.dst != "default"
        && route.prefsrc.is_some()
        && route.protocol.as_deref() == Some("kernel")
}

fn parse_net_params_from_str(ifa_name: &str, routes_json_str: &str) -> Result<NetParams> {
    let mut routes: Vec<IpRoute> = serde_json::from_str(routes_json_str)?;
    let mut params = NetParams::default();
    if let Some(route) = routes.iter().find(|route| route.dst == "default") {
        params.gateway.clone_from(&route.gateway);
    }
    routes.retain(|route| is_dev_ip(route, ifa_name));
    if let Some(route) = routes.pop() {
        // if multiple IPs, let user decide
        if routes.is_empty() {
            // IP range available for VMs
            let cidr = Ipv4Cidr::from_str(&route.dst)
                .with_context(|| format!("cannot parse {} as cidr", route.dst))?;
            let mut ips = cidr.iter();
            if cidr.get_bits() <= 30 {
                // For routing mask values <= 30, first and last IPs are
                // base and broadcast addresses and are unusable.
                ips.next();
                ips.next_back();
            }
            params.ip_from = ips.next().map(|u| Ipv4Addr::from(u).to_string());
            params.ip_to = ips.next_back().map(|u| Ipv4Addr::from(u).to_string());
            // Host IP address
            params.ip = route.prefsrc;
        }
    }
    Ok(params)
}

pub async fn discover_net_params(ifa_name: &str) -> Result<NetParams> {
    let routes = run_cmd("ip", ["--json", "route"]).await?;
    parse_net_params_from_str(ifa_name, &routes)
}

pub fn render_template(template: &str, destination: &Path, params: &[(&str, &str)]) -> Result<()> {
    let mut context = tera::Context::new();
    for (key, value) in params {
        context.insert(key.to_string(), value);
    }
    let mut tera = tera::Tera::default();
    tera.add_raw_template("template", template)?;
    let destination_file = std::fs::File::create(destination)?;
    tera.render_to("template", &context, destination_file)?;
    Ok(())
}

pub fn node_id_with_fallback(node_id: Option<String>) -> Result<String> {
    Ok(match node_id {
        None => {
            if let Ok(workspace::Workspace {
                active_node: Some(workspace::ActiveNode { id, .. }),
                ..
            }) = workspace::read(&std::env::current_dir()?)
            {
                id.to_string()
            } else {
                bail!("<ID_OR_NAME> neither provided nor found in the workspace");
            }
        }
        Some(id) => id,
    })
}

pub fn node_ids_with_fallback(mut node_ids: Vec<String>, required: bool) -> Result<Vec<String>> {
    if node_ids.is_empty() {
        if let Ok(workspace::Workspace {
            active_node: Some(workspace::ActiveNode { id, .. }),
            ..
        }) = workspace::read(&std::env::current_dir()?)
        {
            node_ids.push(id.to_string());
        } else if required {
            bail!("<ID_OR_NAMES> neither provided nor found in the workspace");
        }
    }
    Ok(node_ids)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    pub fn test_semver_sort() {
        let mut versions = vec![
            "1.2.0",
            "1.2.3+2",
            "1.2.3+10",
            "1.3.0",
            "2.0.0",
            "1.0.0-build.3",
            "1.0.0-build.20",
            "1.0.0-build.100",
            "1.0.0-alpha.1",
            "1.0.0-1",
            "1.0.0-beta.1",
            "1.0.0-beta",
            "1.0.0",
            "not",
            "being",
            "sorted",
            "0",
            "1",
            "3.4.0_bad_underscore.3",
            "3.4.0_bad_underscore.10",
        ];
        versions.sort_by(|a, b| semver_cmp(b, a));

        assert_eq!(
            versions,
            vec![
                "2.0.0",
                "1.3.0",
                "1.2.3+10",
                "1.2.3+2",
                "1.2.0",
                "1.0.0",
                "1.0.0-build.100",
                "1.0.0-build.20",
                "1.0.0-build.3",
                "1.0.0-beta.1",
                "1.0.0-beta",
                "1.0.0-alpha.1",
                "1.0.0-1",
                "not",
                "being",
                "sorted",
                "0",
                "1",
                "3.4.0_bad_underscore.3",
                "3.4.0_bad_underscore.10",
            ]
        );
    }

    #[test]
    fn test_parse_net_params_from_str() {
        let json = r#"[
            {
               "dev" : "bvbr0",
               "dst" : "default",
               "flags" : [],
               "gateway" : "192.69.220.81",
               "protocol" : "static"
            },
            {
               "dev" : "bvbr0",
               "dst" : "192.69.220.80/28",
               "flags" : [],
               "prefsrc" : "192.69.220.82",
               "protocol" : "kernel",
               "scope" : "link"
            }
         ]
         "#;
        let expected = NetParams {
            ip: Some("192.69.220.82".to_string()),
            gateway: Some("192.69.220.81".to_string()),
            ip_from: Some("192.69.220.81".to_string()),
            ip_to: Some("192.69.220.94".to_string()),
        };
        assert_eq!(parse_net_params_from_str("bvbr0", json).unwrap(), expected);
    }
}
