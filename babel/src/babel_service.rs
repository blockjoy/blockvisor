use crate::{jobs_manager::JobsManagerClient, ufw_wrapper::apply_firewall_config, utils};
use async_trait::async_trait;
use babel_api::{
    babel::BlockchainKey,
    engine::{JobConfig, JobStatus},
    metadata::{firewall, BabelConfig, KeysConfig},
};
use eyre::{bail, eyre, Context, ContextCompat, Report, Result};
use serde_json::json;
use std::{
    mem,
    ops::{Deref, DerefMut},
    path::Path,
    path::PathBuf,
    process::Output,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    fs,
    fs::{DirBuilder, File},
    io::AsyncWriteExt,
    sync::{broadcast, oneshot, Mutex, RwLock},
};
use tonic::{Request, Response, Status, Streaming};

const WILDCARD_KEY_NAME: &str = "*";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Lock used to avoid reading job runner binary while it is modified.
/// It stores CRC32 checksum of the binary file.
pub type JobRunnerLock = Arc<RwLock<Option<u32>>>;

pub type LogsTx = oneshot::Sender<broadcast::Sender<String>>;
pub type LogsRx = broadcast::Receiver<String>;

#[derive(Error, Debug)]
pub enum MountError {
    #[error("drive {data_drive_path} already mounted into {data_directory_mount_point}")]
    AlreadyMounted {
        data_drive_path: String,
        data_directory_mount_point: String,
    },
    #[error("failed to mount {data_drive_path} into {data_directory_mount_point} with: {out:?}")]
    MountFailed {
        data_drive_path: String,
        data_directory_mount_point: String,
        out: Output,
    },
    #[error("failed to mount {data_drive_path} into {data_directory_mount_point} with: {err}")]
    Internal {
        data_drive_path: String,
        data_directory_mount_point: String,
        err: Report,
    },
}

/// Trait that allows to inject custom PAL implementation.
#[async_trait]
pub trait BabelPal {
    async fn mount_data_drive(&self, data_directory_mount_point: &str) -> Result<(), MountError>;
    async fn set_hostname(&self, hostname: &str) -> Result<()>;
    async fn set_swap_file(&self, swap_size_mb: usize) -> Result<()>;
}

pub enum BabelStatus {
    Uninitialized(LogsTx),
    Ready(LogsRx),
}

pub struct BabelService<J, P> {
    inner: reqwest::Client,
    status: Arc<Mutex<BabelStatus>>,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: PathBuf,
    /// jobs manager client used to work with jobs
    jobs_manager: J,
    babel_cfg_path: PathBuf,
    pal: P,
}

impl<J, P> Deref for BabelService<J, P> {
    type Target = reqwest::Client;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[async_trait]
impl<J: JobsManagerClient + Sync + Send + 'static, P: BabelPal + Sync + Send + 'static>
    babel_api::babel::babel_server::Babel for BabelService<J, P>
{
    async fn setup_babel(
        &self,
        request: Request<(String, BabelConfig)>,
    ) -> Result<Response<()>, Status> {
        let mut status = self.status.lock().await;
        if let BabelStatus::Uninitialized(_) = status.deref() {
            let (hostname, config) = request.into_inner();

            self.save_babel_conf(&config).await?;

            self.pal
                .set_swap_file(config.swap_size_mb)
                .await
                .map_err(|err| Status::internal(format!("failed to add swap file with: {err}")))?;

            self.pal
                .set_hostname(&hostname)
                .await
                .map_err(|err| Status::internal(format!("failed to setup hostname with: {err}")))?;

            self.pal
                .mount_data_drive(&config.data_directory_mount_point)
                .await
                .map_err(|err| match err {
                    MountError::AlreadyMounted { .. } => {
                        Status::already_exists(eyre!("{err}").to_string())
                    }
                    _ => Status::internal(eyre!("{err}").to_string()),
                })?;

            // setup logs_server
            let (logs_broadcast_tx, logs_rx) = broadcast::channel(config.log_buffer_capacity_ln);
            if let BabelStatus::Uninitialized(logs_tx) =
                mem::replace(status.deref_mut(), BabelStatus::Ready(logs_rx))
            {
                logs_tx
                    .send(logs_broadcast_tx)
                    .map_err(|_| Status::internal("failed to setup logs_server"))?;
            } else {
                unreachable!()
            }
        }
        Ok(Response::new(()))
    }

    async fn setup_firewall(
        &self,
        request: Request<firewall::Config>,
    ) -> Result<Response<()>, Status> {
        apply_firewall_config(request.into_inner())
            .await
            .map_err(|err| {
                Status::internal(format!("failed to apply firewall config with: {err}"))
            })?;
        Ok(Response::new(()))
    }

    async fn download_keys(
        &self,
        request: Request<KeysConfig>,
    ) -> Result<Response<Vec<BlockchainKey>>, Status> {
        let mut results = vec![];

        for (name, location) in request.into_inner().iter() {
            // TODO: open questions about keys download:
            // should we bail if some key does not exist?
            // should we return files from star dir? (potentially not secure)
            if name != WILDCARD_KEY_NAME && Path::new(&location).exists() {
                let content = fs::read(location).await?;
                results.push(BlockchainKey {
                    name: name.clone(),
                    content,
                })
            }
        }

        Ok(Response::new(results))
    }

    async fn upload_keys(
        &self,
        request: Request<(KeysConfig, Vec<BlockchainKey>)>,
    ) -> Result<Response<String>, Status> {
        let (config, keys) = request.into_inner();
        if keys.is_empty() {
            return Err(Status::invalid_argument(
                "Keys management error: No keys provided",
            ));
        }

        if !config.contains_key(WILDCARD_KEY_NAME) {
            for key in &keys {
                let name = &key.name;
                if !config.contains_key(name) {
                    return Err(Status::not_found(format!(
                        "Keys management error: Key `{name}` not found in `keys` config"
                    )));
                }
            }
        }

        let mut results: Vec<String> = vec![];
        for key in &keys {
            let name = &key.name;

            // Calculate destination file name
            // Use location as is, if key is recognized
            // If key is not recognized, but there is a star dir, put file into dir
            let (filename, parent_dir) = if let Some(location) = config.get(name) {
                let location = Path::new(location);
                (location.to_path_buf(), location.parent())
            } else {
                let location = config.get(WILDCARD_KEY_NAME).unwrap(); // checked
                let location = Path::new(location);
                (location.join(name), Some(location))
            };
            if let Some(parent) = parent_dir {
                DirBuilder::new().recursive(true).create(parent).await?;
            }

            // Write key content into file
            let mut f = File::create(filename.clone()).await?;
            f.write_all(&key.content).await?;
            let count = key.content.len();
            results.push(format!(
                "Done writing {count} bytes of key `{name}` into `{}`",
                filename.to_string_lossy()
            ));
        }

        Ok(Response::new(results.join("\n")))
    }

    async fn check_job_runner(
        &self,
        request: Request<u32>,
    ) -> Result<Response<babel_api::utils::BinaryStatus>, Status> {
        let expected_checksum = request.into_inner();
        let job_runner_status = match *self.job_runner_lock.read().await {
            Some(checksum) if checksum == expected_checksum => babel_api::utils::BinaryStatus::Ok,
            Some(_) => babel_api::utils::BinaryStatus::ChecksumMismatch,
            None => babel_api::utils::BinaryStatus::Missing,
        };
        Ok(Response::new(job_runner_status))
    }

    async fn upload_job_runner(
        &self,
        request: Request<Streaming<babel_api::utils::Binary>>,
    ) -> Result<Response<()>, Status> {
        let mut stream = request.into_inner();
        // block using job runner binary
        let mut lock = self.job_runner_lock.write().await;
        let checksum = utils::save_bin_stream(&self.job_runner_bin_path, &mut stream)
            .await
            .map_err(|err| Status::internal(format!("upload_job_runner failed: {err}")))?;
        lock.replace(checksum);
        Ok(Response::new(()))
    }

    async fn start_job(
        &self,
        request: Request<(String, JobConfig)>,
    ) -> Result<Response<()>, Status> {
        let (name, config) = request.into_inner();
        self.jobs_manager
            .start(&name, config)
            .await
            .map_err(|err| Status::internal(format!("start_job failed: {err}")))?;
        Ok(Response::new(()))
    }

    async fn stop_job(&self, request: Request<String>) -> Result<Response<()>, Status> {
        self.jobs_manager
            .stop(&request.into_inner())
            .await
            .map_err(|err| Status::internal(format!("stop_job failed: {err}")))?;
        Ok(Response::new(()))
    }

    async fn job_status(&self, request: Request<String>) -> Result<Response<JobStatus>, Status> {
        let status = self
            .jobs_manager
            .status(&request.into_inner())
            .await
            .map_err(|err| Status::internal(format!("job_status failed: {err}")))?;
        Ok(Response::new(status))
    }

    async fn run_jrpc(
        &self,
        request: Request<(String, String)>,
    ) -> Result<Response<String>, Status> {
        Ok(Response::new(
            self.handle_jrpc(request).await.map_err(to_blockchain_err)?,
        ))
    }

    async fn run_rest(&self, request: Request<String>) -> Result<Response<String>, Status> {
        Ok(Response::new(
            self.handle_rest(request).await.map_err(to_blockchain_err)?,
        ))
    }

    async fn run_sh(&self, request: Request<String>) -> Result<Response<String>, Status> {
        Ok(Response::new(
            self.handle_sh(request).await.map_err(to_blockchain_err)?,
        ))
    }

    async fn render_template(
        &self,
        request: Request<(PathBuf, PathBuf, String)>,
    ) -> Result<Response<()>, Status> {
        let (template, output, params) = request.into_inner();
        let render = || -> Result<()> {
            let params: serde_json::Value = serde_json::from_str(&params)?;
            let context = tera::Context::from_serialize(params)?;
            let mut tera = tera::Tera::default();
            tera.add_template_file(template, Some("template"))?;
            let out_file = std::fs::File::create(output)?;
            tera.render_to("template", &context, out_file)?;
            Ok(())
        };
        render().map_err(|err| {
            Status::internal(format!("failed to render template file with: {err}"))
        })?;
        Ok(Response::new(()))
    }

    type GetLogsStream = tokio_stream::Iter<std::vec::IntoIter<Result<String, Status>>>;

    async fn get_logs(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::GetLogsStream>, Status> {
        let mut logs = Vec::default();
        if let BabelStatus::Ready(rx) = self.status.lock().await.deref_mut() {
            loop {
                match rx.try_recv() {
                    Ok(log) => logs.push(Ok(log)),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        }
        let stream = tokio_stream::iter(logs);
        Ok(Response::new(stream))
    }
}

fn to_blockchain_err(err: eyre::Error) -> Status {
    Status::internal(err.to_string())
}

impl<J, P> BabelService<J, P> {
    pub async fn new(
        job_runner_lock: JobRunnerLock,
        job_runner_bin_path: PathBuf,
        jobs_manager: J,
        babel_cfg_path: PathBuf,
        pal: P,
        status: BabelStatus,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;

        Ok(Self {
            inner: client,
            status: Arc::new(Mutex::new(status)),
            job_runner_lock,
            job_runner_bin_path,
            jobs_manager,
            babel_cfg_path,
            pal,
        })
    }

    async fn save_babel_conf(&self, config: &BabelConfig) -> Result<(), Status> {
        // write config to file in case of babel crash (will be restarted by babelsup)
        let cfg_str = serde_json::to_string(config)
            .map_err(|err| Status::internal(format!("failed to serialize babel config: {err}")))?;
        let _ = fs::remove_file(&self.babel_cfg_path).await;
        fs::write(&self.babel_cfg_path, &cfg_str)
            .await
            .map_err(|err| {
                Status::internal(format!(
                    "failed to save babel config into {}: {}",
                    &self.babel_cfg_path.to_string_lossy(),
                    err
                ))
            })?;
        Ok(())
    }

    async fn handle_jrpc(&self, request: Request<(String, String)>) -> Result<String> {
        let timeout = extract_timeout(&request);
        let (host, method) = request.into_inner();
        let mut req_builder = self
            .post(host)
            .json(&json!({ "jsonrpc": "2.0", "id": 0, "method": method }));
        if let Ok(timeout) = timeout {
            req_builder = req_builder.timeout(timeout);
        }
        Ok(req_builder.send().await?.text().await?)
    }

    async fn handle_rest(&self, request: Request<String>) -> Result<String> {
        let timeout = extract_timeout(&request);
        let url = request.into_inner();
        let mut req_builder = self.get(url);
        if let Ok(timeout) = timeout {
            req_builder = req_builder.timeout(timeout);
        }
        Ok(req_builder.send().await?.text().await?)
    }

    async fn handle_sh(&self, request: Request<String>) -> Result<String> {
        let timeout = extract_timeout(&request);
        let body = request.into_inner();
        let args = vec!["-c", &body];
        let cmd_future = tokio::process::Command::new("sh")
            .kill_on_drop(true)
            .args(args)
            .output();
        let output = if let Ok(timeout) = timeout {
            tokio::time::timeout(timeout, cmd_future).await?
        } else {
            cmd_future.await
        }?;
        if !output.status.success() {
            bail!("Failed to run command `{body}`, got output `{output:?}`")
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Extract request timeout value from "grpc-timeout" header. See [tonic::Request::set_timeout](https://docs.rs/tonic/latest/tonic/struct.Request.html#method.set_timeout).
/// Units are translated according to [gRPC Spec](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests).
fn extract_timeout<T>(req: &Request<T>) -> Result<Duration> {
    let timeout = req
        .metadata()
        .get("grpc-timeout")
        .with_context(|| "grpc-timeout header not set")?
        .to_str()?;
    if !timeout.is_empty() {
        let (value, unit) = timeout.split_at(timeout.len() - 1);
        let value = value
            .parse::<u64>()
            .with_context(|| format!("invalid timeout value: {value}"))?;
        match unit {
            "H" => Ok(Duration::from_secs(value * 3600)),
            "M" => Ok(Duration::from_secs(value * 60)),
            "S" => Ok(Duration::from_secs(value)),
            "m" => Ok(Duration::from_millis(value)),
            "u" => Ok(Duration::from_micros(value)),
            "n" => Ok(Duration::from_nanos(value)),
            _ => bail!("invalid unit: {unit}"),
        }
    } else {
        bail!("empty timeout value")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use babel_api::babel::{babel_client::BabelClient, babel_server::Babel};
    use futures::StreamExt;
    use httpmock::prelude::*;
    use mockall::*;
    use std::collections::HashMap;
    use std::env::temp_dir;
    use tokio::net::UnixStream;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::{Channel, Endpoint, Server, Uri};

    mock! {
        pub JobsManager {}

        #[async_trait]
        impl JobsManagerClient for JobsManager {
            async fn start(&self, name: &str, config: JobConfig) -> Result<()>;
            async fn stop(&self, name: &str) -> Result<()>;
            async fn status(&self, name: &str) -> Result<JobStatus>;
        }
    }

    struct DummyPal;

    #[async_trait]
    impl BabelPal for DummyPal {
        async fn mount_data_drive(
            &self,
            _data_directory_mount_point: &str,
        ) -> Result<(), MountError> {
            Ok(())
        }

        async fn set_hostname(&self, _hostname: &str) -> eyre::Result<()> {
            Ok(())
        }

        async fn set_swap_file(&self, _swap_size_mb: usize) -> eyre::Result<()> {
            Ok(())
        }
    }

    async fn babel_server(
        job_runner_lock: JobRunnerLock,
        job_runner_bin_path: PathBuf,
        uds_stream: UnixListenerStream,
        babel_cfg_path: PathBuf,
        setup: BabelStatus,
    ) -> Result<()> {
        let babel_service = BabelService::new(
            job_runner_lock,
            job_runner_bin_path,
            MockJobsManager::new(),
            babel_cfg_path,
            DummyPal,
            setup,
        )
        .await?;
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(babel_api::babel::babel_server::BabelServer::new(
                babel_service,
            ))
            .serve_with_incoming(uds_stream)
            .await?;
        Ok(())
    }

    fn test_client(tmp_root: &Path) -> Result<BabelClient<Channel>> {
        let socket_path = tmp_root.join("test_socket");
        let channel = Endpoint::from_static("http://[::]:50052")
            .timeout(Duration::from_secs(1))
            .connect_timeout(Duration::from_secs(1))
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                UnixStream::connect(socket_path.clone())
            }));
        Ok(BabelClient::new(channel))
    }

    struct TestEnv {
        job_runner_bin_path: PathBuf,
        job_runner_lock: JobRunnerLock,
        client: BabelClient<Channel>,
        logs_rx: oneshot::Receiver<broadcast::Sender<String>>,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        std::fs::create_dir_all(&tmp_root)?;
        let job_runner_path = tmp_root.join("job_runner");
        let babel_cfg_path = tmp_root.join("babel.cfg");
        let lock = Arc::new(RwLock::new(None));
        let client = test_client(&tmp_root)?;
        let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(
            tmp_root.join("test_socket"),
        )?);
        let job_runner_lock = lock.clone();
        let job_runner_bin_path = job_runner_path.clone();
        let (logs_tx, logs_rx) = oneshot::channel();
        tokio::spawn(async move {
            babel_server(
                lock,
                job_runner_path,
                uds_stream,
                babel_cfg_path,
                BabelStatus::Uninitialized(logs_tx),
            )
            .await
        });

        Ok(TestEnv {
            job_runner_bin_path,
            job_runner_lock,
            client,
            logs_rx,
        })
    }

    async fn build_babel_service_with_defaults() -> Result<BabelService<MockJobsManager, DummyPal>>
    {
        let (_tx, rx) = broadcast::channel(1);
        BabelService::new(
            Arc::new(Default::default()),
            Default::default(),
            MockJobsManager::new(),
            Default::default(),
            DummyPal,
            BabelStatus::Ready(rx),
        )
        .await
    }

    #[tokio::test]
    async fn test_extract_timeout() -> Result<()> {
        let mut req = Request::new("text".to_string());
        assert_eq!(
            "grpc-timeout header not set",
            extract_timeout(&req).unwrap_err().to_string()
        );
        req.set_timeout(Duration::from_secs(7));
        assert_eq!(Duration::from_secs(7), extract_timeout(&req)?);
        req.set_timeout(Duration::from_nanos(77));
        assert_eq!(Duration::from_nanos(77), extract_timeout(&req)?);
        Ok(())
    }

    #[tokio::test]
    async fn test_upload_download_keys() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_dir_str = format!("{}", tmp_dir.to_string_lossy());

        let service = build_babel_service_with_defaults().await?;
        let cfg = HashMap::from([
            ("first".to_string(), format!("{tmp_dir_str}/first/key")),
            ("second".to_string(), format!("{tmp_dir_str}/second/key")),
            ("third".to_string(), format!("{tmp_dir_str}/third/key")),
        ]);

        println!("no files uploaded yet");
        let keys = service
            .download_keys(Request::new(cfg.clone()))
            .await?
            .into_inner();
        assert_eq!(keys.len(), 0);

        println!("upload bad keys");
        let status = service
            .upload_keys(Request::new((cfg.clone(), vec![])))
            .await
            .err()
            .unwrap();
        assert_eq!(status.message(), "Keys management error: No keys provided");

        println!("upload good keys");
        let output = service
            .upload_keys(Request::new((
                cfg.clone(),
                vec![
                    BlockchainKey {
                        name: "second".to_string(),
                        content: b"123".to_vec(),
                    },
                    BlockchainKey {
                        name: "third".to_string(),
                        content: b"abcd".to_vec(),
                    },
                ],
            )))
            .await?
            .into_inner();
        assert_eq!(
            output,
            format!(
                "Done writing 3 bytes of key `second` into `{tmp_dir_str}/second/key`\n\
                 Done writing 4 bytes of key `third` into `{tmp_dir_str}/third/key`"
            )
        );

        println!("download uploaded keys");
        let mut keys = service
            .download_keys(Request::new(cfg.clone()))
            .await?
            .into_inner();
        assert_eq!(keys.len(), 2);
        keys.sort_by_key(|k| k.name.clone());
        assert_eq!(
            keys[0],
            BlockchainKey {
                name: "second".to_string(),
                content: b"123".to_vec(),
            }
        );
        assert_eq!(
            keys[1],
            BlockchainKey {
                name: "third".to_string(),
                content: b"abcd".to_vec(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_upload_download_keys_with_star() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_dir_str = format!("{}", tmp_dir.to_string_lossy());

        let cfg = HashMap::from([(
            WILDCARD_KEY_NAME.to_string(),
            format!("{tmp_dir_str}/star/"),
        )]);
        let service = build_babel_service_with_defaults().await?;

        println!("upload unknown keys");
        let output = service
            .upload_keys(Request::new((
                cfg.clone(),
                vec![BlockchainKey {
                    name: "unknown".to_string(),
                    content: b"12345".to_vec(),
                }],
            )))
            .await?
            .into_inner();

        assert_eq!(
            output,
            format!("Done writing 5 bytes of key `unknown` into `{tmp_dir_str}/star/unknown`")
        );

        println!("files in star dir should not be downloading");
        let keys = service
            .download_keys(Request::new(cfg.clone()))
            .await?
            .into_inner();
        assert_eq!(keys.len(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_jrpc_json_ok() -> Result<()> {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/")
                .header("Content-Type", "application/json")
                .json_body(json!({
                    "id": 0,
                    "jsonrpc": "2.0",
                    "method": "info_get",
                }));
            then.status(200)
                .header("Content-Type", "application/json")
                .json_body(json!({
                        "id": 0,
                        "jsonrpc": "2.0",
                        "result": {"info": {"height": 123, "address": "abc"}},
                }));
        });

        let service = build_babel_service_with_defaults().await?;
        let output = service
            .run_jrpc(Request::new((
                format!("http://{}", server.address()),
                "info_get".to_string(),
            )))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(
            output,
            r#"{"id":0,"jsonrpc":"2.0","result":{"info":{"address":"abc","height":123}}}"#
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_rest_json_ok() -> Result<()> {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET).path("/items");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"result": [1, 2, 3]}));
        });

        let service = build_babel_service_with_defaults().await?;
        let output = service
            .run_rest(Request::new(format!("http://{}/items", server.address())))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(output, r#"{"result":[1,2,3]}"#);
        Ok(())
    }

    #[tokio::test]
    async fn test_rest_json_full_response_ok() -> Result<()> {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET).path("/items");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"result": [1, 2, 3]}));
        });

        let service = build_babel_service_with_defaults().await?;
        let output = service
            .run_rest(Request::new(format!("http://{}/items", server.address())))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(output, "{\"result\":[1,2,3]}");
        Ok(())
    }

    #[tokio::test]
    async fn test_sh() -> Result<()> {
        let service = build_babel_service_with_defaults().await?;

        let output = service
            .run_sh(Request::new("echo \\\"make a toast\\\"".to_string()))
            .await?
            .into_inner();
        assert_eq!(output, "\"make a toast\"\n");
        Ok(())
    }

    #[tokio::test]
    async fn test_upload_job_runner() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let incomplete_runner_bin = vec![
            babel_api::utils::Binary::Bin(vec![1, 2, 3, 4, 6, 7, 8, 9, 10]),
            babel_api::utils::Binary::Bin(vec![11, 12, 13, 14, 16, 17, 18, 19, 20]),
            babel_api::utils::Binary::Bin(vec![21, 22, 23, 24, 26, 27, 28, 29, 30]),
        ];

        test_env
            .client
            .upload_job_runner(tokio_stream::iter(incomplete_runner_bin.clone()))
            .await
            .unwrap_err();
        assert!(test_env.job_runner_lock.read().await.is_none());

        let mut invalid_runner_bin = incomplete_runner_bin.clone();
        invalid_runner_bin.push(babel_api::utils::Binary::Checksum(123));
        test_env
            .client
            .upload_job_runner(tokio_stream::iter(invalid_runner_bin))
            .await
            .unwrap_err();
        assert!(test_env.job_runner_lock.read().await.is_none());

        let mut runner_bin = incomplete_runner_bin.clone();
        runner_bin.push(babel_api::utils::Binary::Checksum(4135829304));
        test_env
            .client
            .upload_job_runner(tokio_stream::iter(runner_bin))
            .await?;
        assert_eq!(4135829304, test_env.job_runner_lock.read().await.unwrap());
        assert_eq!(
            4135829304,
            utils::file_checksum(&test_env.job_runner_bin_path)
                .await
                .unwrap()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_check_job_runner() -> Result<()> {
        let job_runner_bin_path = TempDir::new().unwrap().join("job_runner");
        let job_runner_lock = Arc::new(RwLock::new(None));

        let (_, rx) = broadcast::channel(1);
        let service = BabelService::new(
            job_runner_lock.clone(),
            job_runner_bin_path,
            MockJobsManager::new(),
            Default::default(),
            DummyPal,
            BabelStatus::Ready(rx),
        )
        .await?;

        assert_eq!(
            babel_api::utils::BinaryStatus::Missing,
            service
                .check_job_runner(Request::new(123))
                .await?
                .into_inner()
        );

        job_runner_lock.write().await.replace(321);
        assert_eq!(
            babel_api::utils::BinaryStatus::ChecksumMismatch,
            service
                .check_job_runner(Request::new(123))
                .await?
                .into_inner()
        );
        assert_eq!(
            babel_api::utils::BinaryStatus::Ok,
            service
                .check_job_runner(Request::new(321))
                .await?
                .into_inner()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_render_template() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        fs::create_dir_all(tmp_dir).await?;
        let template_path = temp_dir().join("template.txt");
        fs::write(
            &template_path,
            r#"p1: {{ param1 }}, p2: {{param2}}, p3: {% if param3 %}{{ param3 }}{% else %}None{% endif %}; {% for p in vParam %}({{p}}){% endfor %}"#,
        )
        .await?;
        let out_path = temp_dir().join("out.txt");

        let service = build_babel_service_with_defaults().await?;

        service
            .render_template(Request::new((
                template_path.clone(),
                out_path.clone(),
                r#"{"param1": "value1", "param2": 2, "param3": true, "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await?;
        assert_eq!(
            "p1: value1, p2: 2, p3: true; (a)(bb)(ccc)",
            fs::read_to_string(&out_path).await?
        );

        service
            .render_template(Request::new((
                template_path.clone(),
                out_path.clone(),
                r#"{"param1": "value1", "param2": 2, "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await?;
        assert_eq!(
            "p1: value1, p2: 2, p3: None; (a)(bb)(ccc)",
            fs::read_to_string(&out_path).await?
        );

        service
            .render_template(Request::new((
                template_path.clone(),
                out_path.clone(),
                r#"{"param1": "value1", "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await
            .unwrap_err();

        service
            .render_template(Request::new((
                Path::new("invalid/path").to_path_buf(),
                out_path.clone(),
                r#"{"param1": "value1", "param2": 2, "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await
            .unwrap_err();

        service
            .render_template(Request::new((
                template_path.clone(),
                Path::new("invalid/path").to_path_buf(),
                r#"{"param1": "value1", "param2": 2, "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await
            .unwrap_err();

        Ok(())
    }

    #[tokio::test]
    async fn test_logs() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let mut stream = test_env.client.get_logs(()).await?.into_inner();

        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = stream.next().await {
            logs.push(log);
        }

        assert_eq!(Vec::<String>::default(), logs);

        test_env
            .client
            .setup_babel((
                "localhost".to_string(),
                BabelConfig {
                    data_directory_mount_point: "".to_string(),
                    log_buffer_capacity_ln: 10,
                    swap_size_mb: 16,
                },
            ))
            .await?;
        let logs_tx = test_env.logs_rx.await?;
        logs_tx
            .send("log1".to_string())
            .expect("failed to send log");
        logs_tx
            .send("log2".to_string())
            .expect("failed to send log");
        logs_tx
            .send("log3".to_string())
            .expect("failed to send log");

        let mut stream = test_env.client.get_logs(()).await?.into_inner();

        let mut logs = Vec::<String>::default();
        while let Some(Ok(log)) = stream.next().await {
            logs.push(log);
        }

        assert_eq!(vec!["log1", "log2", "log3"], logs);
        Ok(())
    }
}
