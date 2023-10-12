use bv_utils::cmd::run_cmd;
use cidr_utils::cidr::Ipv4Cidr;
use eyre::{anyhow, bail, Context, Result};
use rand::Rng;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, ffi::OsStr, net::Ipv4Addr, path::PathBuf, time::Duration};
use sysinfo::{PidExt, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use tokio::{fs, io::AsyncWriteExt, time::sleep};
use tonic::Request;
use tracing::{debug, warn};

/// Get the pid of the running VM process knowing its process name and part of command line.
pub fn get_process_pid(process_name: &str, cmd: &str) -> Result<u32> {
    let mut sys = System::new();
    debug!("Retrieving pid for process `{process_name}` and cmd like `{cmd}`");
    // TODO: would be great to save the System and not do a full refresh each time
    sys.refresh_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::everything()));
    let processes: Vec<_> = sys
        .processes_by_name(process_name)
        .filter(|&process| process.cmd().contains(&cmd.to_string()))
        .collect();

    match processes.len() {
        0 => bail!("No {process_name} processes running for id: {cmd}"),
        1 => Ok(processes[0].pid().as_u32()),
        _ => bail!("More then 1 {process_name} process running for id: {cmd}"),
    }
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
    let mut retry_count = 0;
    while let Err(err) = download_file(url, &path).await {
        if retry_count < DOWNLOAD_RETRY_MAX {
            retry_count += 1;
            let backoff = DOWNLOAD_BACKOFF_BASE_SEC * 2u64.pow(retry_count);
            warn!("download failed for {url} with {err}; {retry_count} retry after {backoff}s");
            sleep(Duration::from_secs(backoff)).await;
        } else {
            bail!("download failed for {url} with {err}; retries exceeded");
        }
    }
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

    let mut resp = reqwest::get(url).await?;

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

pub fn with_timeout<T>(args: T, timeout: Duration) -> Request<T> {
    let mut req = Request::new(args);
    req.set_timeout(timeout);
    req
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

/// Create a MAC address based on the provided IP
///
/// Our prefix it CA:92, rest is IPv4 bytes
///
/// Inspired by https://github.com/firecracker-microvm/firecracker/blob/main/tests/host_tools/network.py#L122
pub fn ip_to_mac(ip: &Ipv4Addr) -> String {
    let octets = ip.octets();
    format!(
        "CA:92:{:02X}:{:02X}:{:02X}:{:02X}",
        octets[0], octets[1], octets[2], octets[3]
    )
}

/// Struct to capture output of linux `ip --json route` command
#[derive(Deserialize, Serialize, Debug)]
struct IpRoute {
    dst: String,
    gateway: Option<String>,
    dev: String,
    prefsrc: Option<String>,
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

fn parse_net_params_from_str(ifa_name: &str, routes_json_str: &str) -> Result<NetParams> {
    let mut routes: Vec<IpRoute> = serde_json::from_str(routes_json_str)?;
    routes.retain(|r| r.dev == ifa_name);
    if routes.len() != 2 {
        bail!("Routes count for `{ifa_name}` not equal to 2");
    }

    let mut params = NetParams::default();
    for route in routes {
        if route.dst == "default" {
            // Host gateway IP address
            params.gateway = route.gateway;
        } else {
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

#[cfg(test)]
pub mod tests {
    use super::*;
    use http::{Request, Response};
    use hyper::Body;
    use std::convert::Infallible;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{atomic, Arc};
    use std::thread::panicking;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::task::JoinHandle;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::body::BoxBody;
    use tonic::codegen::Service;
    use tonic::transport::{Channel, Endpoint, NamedService, Server, Uri};

    #[test]
    pub fn test_ip_to_mac() {
        assert_eq!(
            ip_to_mac(&"192.168.241.2".parse().unwrap()),
            "CA:92:C0:A8:F1:02".to_string()
        );
    }

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

    pub fn test_channel(tmp_root: &Path) -> Channel {
        let socket_path = tmp_root.join("test_socket");
        Endpoint::from_static("http://[::]:50052")
            .timeout(Duration::from_secs(1))
            .connect_timeout(Duration::from_secs(1))
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                UnixStream::connect(socket_path.clone())
            }))
    }

    /// Helper struct that adds a panic hook and checks if it was called on `Drop`.
    /// It is needed when a mock object is moved to another (e.g. server) thread.
    /// By default panics from threads different from main test thread are suppressed,
    /// and tests pass even if Mock assertion fails. Creating an `AsyncPanicChecker` struct in a test will
    /// make sure that the test fails in such case.
    pub struct AsyncPanicChecker {
        flag: Arc<AtomicBool>,
    }

    impl Drop for AsyncPanicChecker {
        fn drop(&mut self) {
            if !panicking() {
                assert!(!self.flag.load(atomic::Ordering::Relaxed));
            }
        }
    }

    impl Default for AsyncPanicChecker {
        fn default() -> Self {
            let flag = Arc::new(AtomicBool::new(false));
            let async_panic = flag.clone();
            let default_panic = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                default_panic(info);
                async_panic.store(true, atomic::Ordering::Relaxed);
            }));
            Self { flag }
        }
    }

    /// Helper struct to gracefully shutdown and join the test server,
    /// to make sure all mock asserts are checked.
    pub struct TestServer {
        pub handle: JoinHandle<()>,
        pub tx: tokio::sync::oneshot::Sender<()>,
    }

    impl TestServer {
        pub async fn assert(self) {
            let _ = self.tx.send(());
            let _ = self.handle.await;
        }
    }

    pub fn start_test_server<S>(socket_path: PathBuf, service_mock: S) -> TestServer
    where
        S: Service<Request<Body>, Response = Response<BoxBody>, Error = Infallible>
            + NamedService
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        TestServer {
            tx,
            handle: tokio::spawn(async move {
                let uds_stream =
                    UnixListenerStream::new(tokio::net::UnixListener::bind(socket_path).unwrap());
                Server::builder()
                    .max_concurrent_streams(1)
                    .add_service(service_mock)
                    .serve_with_incoming_shutdown(uds_stream, async {
                        rx.await.ok();
                    })
                    .await
                    .unwrap();
            }),
        }
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
