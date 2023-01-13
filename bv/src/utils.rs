use anyhow::{bail, Context, Result};
use semver::Version;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Display;
use std::path::PathBuf;
use sysinfo::{PidExt, ProcessExt, ProcessRefreshKind, RefreshKind, System, SystemExt};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info};

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
        .with_context(|| format!("Failed to run command `{:?}`", cmd))?
        .code()
    {
        Some(code) if code != 0 => bail!("Command `{:?}` failed with exit code {}", cmd, code),
        Some(_) => Ok(()),
        None => bail!("Command `{:?}` failed with no exit code", cmd),
    }
}

/// Get the pid of the running VM process knowing its process name and part of command line.
pub fn get_process_pid(process_name: &str, cmd: &str) -> Result<i32> {
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
        1 => processes[0].pid().as_u32().try_into().map_err(Into::into),
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

pub async fn download_archive(url: &str, path: PathBuf) -> Result<Archive> {
    debug!("Downloading url...");
    let mut file = fs::File::create(&path).await?;

    let mut resp = reqwest::get(url).await?;

    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
    }

    file.flush().await?;
    debug!("Done downloading");

    Ok(Archive(path))
}

pub fn semver_cmp(a: &str, b: &str) -> Ordering {
    match (Version::parse(a), Version::parse(b)) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        (Ok(_), Err(_)) => Ordering::Greater,
        (Err(_), Ok(_)) => Ordering::Less,
        (Err(_), Err(_)) => Ordering::Equal,
    }
}

/// Renders a template by filling in uppercased, `{{ }}`-delimited template strings with the
/// values in the `params` dictionary.
pub fn render(template: &str, params: &HashMap<impl Display, impl Display>) -> String {
    let mut res = template.to_string();
    for (key, value) in params {
        // This formats a parameter like `url` as `{{URL}}`
        let placeholder = format!("{{{{{}}}}}", key.to_string().to_uppercase());
        res = res.replace(&placeholder, &value.to_string());
    }
    res
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
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::task::JoinHandle;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::body::BoxBody;
    use tonic::codegen::Service;
    use tonic::transport::{Channel, Endpoint, NamedService, Server, Uri};

    pub fn test_channel(tmp_root: &Path) -> Channel {
        let socket_path = tmp_root.join("test_socket");
        Endpoint::try_from("http://[::]:50052")
            .unwrap()
            .timeout(Duration::from_secs(1))
            .connect_timeout(Duration::from_secs(1))
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                UnixStream::connect(socket_path.clone())
            }))
    }

    /// Helper struct that add panic hook and check if it was called on `Drop`.
    /// It is needed when mock object is moved to another (e.g. server) thread.
    /// By default panics from threads different than main test thread are suppressed,
    /// and test pass even if Mock assertion fail. Creating `AsyncPanicChecker` struct in test will
    /// make sure that test fail in such case.
    pub struct AsyncPanicChecker {
        flag: Arc<AtomicBool>,
    }

    impl Drop for AsyncPanicChecker {
        fn drop(&mut self) {
            assert!(!self.flag.load(atomic::Ordering::Relaxed));
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

    /// Helper struct to gracefully shutdown and join test server,
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
    fn test_render() {
        let s = |s: &str| s.to_string(); // to make the test less verbose
        let par1 = s("val1");
        let par2 = s("val2");
        let par3 = s("val3 val4");
        let params = [(s("par1"), par1), (s("pAr2"), par2), (s("PAR3"), par3)]
            .into_iter()
            .collect();
        let render = |template| render(template, &params);

        assert_eq!(render("{{PAR1}} bla"), "val1 bla");
        assert_eq!(render("{{PAR2}} waa"), "val2 waa");
        assert_eq!(render("{{PAR3}} kra"), "val3 val4 kra");
        assert_eq!(render("{{par1}} woo"), "{{par1}} woo");
        assert_eq!(render("{{pAr2}} koo"), "{{pAr2}} koo");
        assert_eq!(render("{{PAR3}} doo"), "val3 val4 doo");
    }
}
