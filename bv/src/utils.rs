use anyhow::{bail, Context, Result};
use semver::Version;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;
use sysinfo::{PidExt, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::log::warn;
use tracing::{debug, info};

#[macro_export]
macro_rules! with_retry {
    ($fun:expr) => {{
        const RPC_RETRY_MAX: u32 = 3;
        const RPC_BACKOFF_BASE_MSEC: u64 = 300;
        let mut retry_count = 0;
        loop {
            match $fun.await {
                Ok(res) => break Ok(res),
                Err(err) => {
                    if retry_count < RPC_RETRY_MAX {
                        retry_count += 1;
                        let backoff = RPC_BACKOFF_BASE_MSEC * 2u64.pow(retry_count);
                        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                        continue;
                    } else {
                        break Err(err);
                    }
                }
            }
        }
    }};
}

/// Runs the specified command and returns error on failure.
pub async fn run_cmd<I, S>(cmd: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(cmd);
    cmd.args(args);
    info!("Running command: `{:?}`", cmd);
    match cmd
        .status()
        .await
        .with_context(|| format!("Failed to run command `{cmd:?}`"))?
        .code()
    {
        Some(code) if code != 0 => bail!("Command `{cmd:?}` failed with exit code {code}"),
        Some(_) => Ok(()),
        None => bail!("Command `{cmd:?}` failed with no exit code"),
    }
}

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
        if let Some(parent_dir) = self.0.parent() {
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
        } else {
            bail!("invalid tar file path {}", self.0.to_string_lossy())
        }
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

/// Walks down a toml tree represented by a `toml::Value`. The path that is walked is specified by
/// the `path` argument.
/// ```rs
/// let path = "some.seg.ment.list".split('.');
/// let val: toml::Value = toml::toml!(
/// [some]
/// [some.seg]
/// [some.seg.ment]
/// list = "all the way down here."
/// );
/// assert_eq!("all the way down here.", walk(&val, path).unwrap().as_str().unwrap());
/// ```
fn walk_toml_tree<'a>(
    value: &'a toml::Value,
    mut path: impl Iterator<Item = &'a str>,
) -> Option<&toml::Value> {
    match path.next() {
        Some(seg) => walk_toml_tree(value.get(seg)?, path),
        None => Some(value),
    }
}

/// Get a Babel config value string specified by `path` argument.
/// Path are '.' separated names of sub nodes in the config structure tree.
/// See `walk_toml_tree()` for more.
pub fn get_config_value_by_path(value: &toml::Value, path: &str) -> Option<String> {
    walk_toml_tree(value, path.split('.')).map(|v| {
        if let toml::Value::String(v) = v {
            v.clone()
        } else {
            v.to_string()
        }
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use babel_api::config::{Babel, NetConfiguration, NetType, Requirements};
    use http::{Request, Response};
    use hyper::Body;
    use std::collections::HashMap;
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
    fn test_get_config_value_by_path() -> Result<()> {
        let babel_conf = Babel {
            export: None,
            env: None,
            config: default_config(),
            requirements: Requirements {
                vcpu_count: 7,
                mem_size_mb: 0,
                disk_size_gb: 0,
            },
            nets: HashMap::from([
                (
                    "mainnet".to_string(),
                    NetConfiguration {
                        url: "".to_string(),
                        net_type: NetType::Main,
                        meta: HashMap::from([(
                            "blockchain_custom".to_string(),
                            "blockchain_custom.mainnet_Value".to_string(),
                        )]),
                    },
                ),
                (
                    "testnet".to_string(),
                    NetConfiguration {
                        url: "".to_string(),
                        net_type: NetType::Test,
                        meta: HashMap::from([(
                            "blockchain_custom".to_string(),
                            "blockchain_custom_testnet.Value".to_string(),
                        )]),
                    },
                ),
            ]),
            supervisor: Default::default(),
            firewall: None,
            keys: None,
            methods: Default::default(),
        };
        let babel_conf = toml::Value::try_from(babel_conf).unwrap();

        assert_eq!(
            "blockchain_custom.mainnet_Value",
            get_config_value_by_path(&babel_conf, "nets.mainnet.blockchain_custom").unwrap()
        );
        assert_eq!(
            "7",
            get_config_value_by_path(&babel_conf, "requirements.vcpu_count").unwrap()
        );
        assert!(get_config_value_by_path(&babel_conf, "some.invalid_field").is_none());
        Ok(())
    }

    pub fn default_config() -> babel_api::Config {
        babel_api::Config {
            min_babel_version: "".to_string(),
            node_version: "".to_string(),
            protocol: "".to_string(),
            node_type: "".to_string(),
            description: None,
            api_host: None,
        }
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

    #[test]
    fn test_walk_toml_tree() {
        let path = "some.seg.ment.list".split('.');
        let val: toml::Value = toml::toml!(
        [some]
        [some.seg]
        [some.seg.ment]
        list = "all the way down here."
        );
        assert_eq!(
            "all the way down here.",
            walk_toml_tree(&val, path).unwrap().as_str().unwrap()
        );
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
}
