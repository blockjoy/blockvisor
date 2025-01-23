use crate::{
    apply_babel_config, jobs_manager::JobsManagerClient, load_config, pal::BabelPal, utils,
};
use async_trait::async_trait;
use babel_api::{
    engine::{HttpResponse, JobConfig, JobInfo, JobsInfo, JrpcRequest, RestRequest, ShResponse},
    utils::{protocol_data_stamp, BabelConfig},
};
use eyre::{anyhow, ContextCompat, Result};
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    RequestBuilder,
};
use serde_json::json;
use std::{
    ops::Deref,
    path::PathBuf,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    fs,
    sync::{broadcast, oneshot, RwLock},
};
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BABEL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Lock used to avoid reading job runner binary while it is modified.
/// It stores CRC32 checksum of the binary file.
pub type JobRunnerLock = Arc<RwLock<Option<u32>>>;

pub type LogsTx = oneshot::Sender<broadcast::Sender<String>>;
pub type LogsRx = broadcast::Receiver<String>;

pub struct BabelService<J, P> {
    inner: reqwest::Client,
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
    async fn get_version(
        &self,
        _request: Request<()>,
    ) -> std::result::Result<Response<String>, Status> {
        Ok(Response::new(env!("CARGO_PKG_VERSION").to_string()))
    }

    async fn setup_babel(&self, request: Request<BabelConfig>) -> Result<Response<()>, Status> {
        let config = request.into_inner();

        apply_babel_config(&self.pal, &config)
            .await
            .map_err(|err| Status::internal(anyhow!("{err:#}").to_string()))?;
        self.save_babel_conf(&config).await?;
        self.pal
            .setup_node()
            .await
            .map_err(|err| Status::internal(format!("failed to setup node with: {err:#}")))?;
        self.jobs_manager
            .startup(config.node_env)
            .await
            .map_err(|err| Status::internal(format!("failed to startup jobs_manger: {err:#}")))?;
        Ok(Response::new(()))
    }

    async fn get_babel_shutdown_timeout(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Duration>, Status> {
        Ok(Response::new(
            self.jobs_manager.get_active_jobs_shutdown_timeout().await + BABEL_SHUTDOWN_TIMEOUT,
        ))
    }

    async fn shutdown_babel(&self, request: Request<bool>) -> Result<Response<()>, Status> {
        let force = request.into_inner();
        self.jobs_manager
            .shutdown(force)
            .await
            .map_err(|err| Status::internal(format!("failed to shutdown jobs_manger: {err:#}")))?;

        Ok(Response::new(()))
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
            .map_err(|err| Status::internal(format!("upload_job_runner failed: {err:#}")))?;
        lock.replace(checksum);
        Ok(Response::new(()))
    }

    async fn create_job(
        &self,
        request: Request<(String, JobConfig)>,
    ) -> Result<Response<()>, Status> {
        let (name, config) = request.into_inner();
        self.jobs_manager
            .create(&name, config)
            .await
            .map_err(|err| Status::internal(format!("create_job failed: {err:#}")))?;
        Ok(Response::new(()))
    }

    async fn start_job(&self, request: Request<String>) -> Result<Response<()>, Status> {
        let name = request.into_inner();
        self.jobs_manager
            .start(&name)
            .await
            .map_err(|err| Status::internal(format!("start_job failed: {err:#}")))?;
        Ok(Response::new(()))
    }

    async fn stop_job(&self, request: Request<String>) -> Result<Response<()>, Status> {
        self.jobs_manager
            .stop(&request.into_inner())
            .await
            .map_err(|err| Status::internal(format!("stop_job failed: {err:#}")))?;
        Ok(Response::new(()))
    }

    async fn skip_job(&self, request: Request<String>) -> Result<Response<()>, Status> {
        self.jobs_manager
            .skip(&request.into_inner())
            .await
            .map_err(|err| Status::internal(format!("stop_job failed: {err:#}")))?;
        Ok(Response::new(()))
    }

    async fn cleanup_job(&self, request: Request<String>) -> Result<Response<()>, Status> {
        self.jobs_manager
            .cleanup(&request.into_inner())
            .await
            .map_err(|err| Status::internal(format!("cleanup_job failed: {err:#}")))?;
        Ok(Response::new(()))
    }

    async fn job_info(&self, request: Request<String>) -> Result<Response<JobInfo>, Status> {
        let info = self
            .jobs_manager
            .info(&request.into_inner())
            .await
            .map_err(|err| Status::internal(format!("job_status failed: {err:#}")))?;
        Ok(Response::new(info))
    }

    async fn get_job_shutdown_timeout(
        &self,
        request: Request<String>,
    ) -> Result<Response<Duration>, Status> {
        let job = request.into_inner();
        Ok(Response::new(
            self.jobs_manager.get_job_shutdown_timeout(&job).await,
        ))
    }

    async fn get_jobs(&self, _request: Request<()>) -> Result<Response<JobsInfo>, Status> {
        let jobs = self
            .jobs_manager
            .list()
            .await
            .map_err(|err| Status::internal(format!("list jobs failed: {err:#}")))?;
        Ok(Response::new(jobs))
    }

    async fn run_jrpc(
        &self,
        request: Request<JrpcRequest>,
    ) -> Result<Response<HttpResponse>, Status> {
        Ok(Response::new(
            self.handle_jrpc(request).await.map_err(to_protocol_err)?,
        ))
    }

    async fn run_rest(
        &self,
        request: Request<RestRequest>,
    ) -> Result<Response<HttpResponse>, Status> {
        Ok(Response::new(
            self.handle_rest(request).await.map_err(to_protocol_err)?,
        ))
    }

    async fn run_sh(&self, request: Request<String>) -> Result<Response<ShResponse>, Status> {
        Ok(Response::new(
            self.handle_sh(request).await.map_err(to_protocol_err)?,
        ))
    }

    async fn render_template(
        &self,
        request: Request<(PathBuf, PathBuf, String)>,
    ) -> Result<Response<()>, Status> {
        let (template, destination, params) = request.into_inner();
        let render = || -> Result<()> {
            let params: serde_json::Value = serde_json::from_str(&params)?;
            let context = tera::Context::from_serialize(params)?;
            let mut tera = tera::Tera::default();
            let template_name = destination
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or("template".to_string());
            tera.add_template_file(template, Some(&template_name))?;
            if let Some(parent) = destination.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let destination_file = std::fs::File::create(destination)?;
            tera.render_to(&template_name, &context, destination_file)?;
            Ok(())
        };
        render().map_err(|err| {
            Status::internal(format!("failed to render template file with: {err:#}"))
        })?;
        Ok(Response::new(()))
    }

    async fn file_write(
        &self,
        request: Request<Streaming<babel_api::utils::Binary>>,
    ) -> Result<Response<()>, Status> {
        let mut stream = request.into_inner();
        let Some(Ok(babel_api::utils::Binary::Destination(path))) = stream.next().await else {
            return Err(Status::internal("missing file destination"));
        };
        utils::save_bin_stream(&path, &mut stream)
            .await
            .map_err(|err| {
                Status::internal(format!("file_write {} failed: {err:#}", path.display()))
            })?;
        Ok(Response::new(()))
    }

    type FileReadStream =
        Pin<Box<dyn Stream<Item = Result<babel_api::utils::Binary, Status>> + Send>>;
    async fn file_read(
        &self,
        request: Request<PathBuf>,
    ) -> Result<Response<Self::FileReadStream>, Status> {
        let path = request.into_inner();
        let (content, _) = bv_utils::system::load_bin(&path).await.map_err(|err| {
            Status::internal(format!("file_read {} failed: {err:#}", path.display()))
        })?;
        Ok(Response::new(Box::pin(tokio_stream::iter(
            content.into_iter().map(Ok).collect::<Vec<_>>(),
        ))))
    }

    async fn protocol_data_stamp(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Option<SystemTime>>, Status> {
        let babel_config = load_config(&self.babel_cfg_path)
            .await
            .map_err(|err| Status::internal(format!("failed to read babel config: {err:#}")))?;
        Ok(Response::new(
            protocol_data_stamp(&babel_config.node_env.data_mount_point).map_err(|err| {
                Status::internal(format!("failed to get protocol data stamp: {err:#}"))
            })?,
        ))
    }
}

fn to_protocol_err(err: eyre::Error) -> Status {
    Status::internal(err.to_string())
}

impl<J, P> BabelService<J, P> {
    pub fn new(
        job_runner_lock: JobRunnerLock,
        job_runner_bin_path: PathBuf,
        jobs_manager: J,
        babel_cfg_path: PathBuf,
        pal: P,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;

        Ok(Self {
            inner: client,
            job_runner_lock,
            job_runner_bin_path,
            jobs_manager,
            babel_cfg_path,
            pal,
        })
    }

    async fn save_babel_conf(&self, config: &BabelConfig) -> Result<(), Status> {
        // write config to file in case of babel crash (will be restarted by recovery)
        let cfg_str = serde_json::to_string(config).map_err(|err| {
            Status::internal(format!("failed to serialize babel config: {err:#}"))
        })?;
        let _ = fs::remove_file(&self.babel_cfg_path).await;
        fs::write(&self.babel_cfg_path, &cfg_str)
            .await
            .map_err(|err| {
                Status::internal(format!(
                    "failed to save babel config into {}: {:#}",
                    &self.babel_cfg_path.to_string_lossy(),
                    err
                ))
            })?;
        Ok(())
    }

    async fn handle_jrpc(&self, request: Request<JrpcRequest>) -> Result<HttpResponse> {
        let timeout = bv_utils::rpc::extract_grpc_timeout(&request);
        let req = request.into_inner();
        let data = match req.params {
            None => json!({ "jsonrpc": "2.0", "id": 0, "method": req.method }),
            Some(p) => {
                let params: serde_json::Value = serde_json::from_str(&p)?;
                json!({ "jsonrpc": "2.0", "id": 0, "method": req.method, "params": params })
            }
        };
        send_http_request(self.post(&req.host).json(&data), req.headers, timeout).await
    }

    async fn handle_rest(&self, request: Request<RestRequest>) -> Result<HttpResponse> {
        let timeout = bv_utils::rpc::extract_grpc_timeout(&request);
        let req = request.into_inner();
        send_http_request(self.get(req.url), req.headers, timeout).await
    }

    async fn handle_sh(&self, request: Request<String>) -> Result<ShResponse> {
        let timeout = bv_utils::rpc::extract_grpc_timeout(&request);
        let body = request.into_inner();
        let (cmd, args) = utils::bv_shell(&body);
        let cmd_future = tokio::process::Command::new(cmd)
            .kill_on_drop(true)
            .args(args)
            .output();
        let output = if let Ok(timeout) = timeout {
            tokio::time::timeout(timeout, cmd_future).await?
        } else {
            cmd_future.await
        }?;
        Ok(ShResponse {
            exit_code: output.status.code().with_context(|| {
                format!("Failed to run command `{body}`, got output `{output:?}`")
            })?,
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

/// Takes RequestBuilder and add common http things (timeout, headers).
/// Then send it and translates result into `HttpResponse`.
async fn send_http_request(
    mut req_builder: RequestBuilder,
    headers: Option<Vec<(String, String)>>,
    timeout: Result<Duration>,
) -> Result<HttpResponse> {
    if let Some(headers) = headers {
        let mut headers_map = HeaderMap::new();
        for (key, value) in headers {
            headers_map.insert(HeaderName::from_str(&key)?, HeaderValue::from_str(&value)?);
        }
        req_builder = req_builder.headers(headers_map);
    }
    if let Ok(timeout) = timeout {
        req_builder = req_builder.timeout(timeout);
    }
    let resp = req_builder.send().await?;
    Ok(HttpResponse {
        status_code: resp.status().as_u16(),
        body: resp.text().await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chroot_platform::{UdsConnector, UdsServer};
    use assert_fs::TempDir;
    use babel_api::babel::{babel_client::BabelClient, babel_server::Babel};
    use babel_api::engine::NodeEnv;
    use babel_api::utils::RamdiskConfiguration;
    use bv_tests_utils::start_test_server;
    use mockall::*;
    use serde_json::json;
    use std::env::temp_dir;
    use std::path::Path;
    use tonic::transport::Channel;

    mock! {
        pub JobsManager {}

        #[async_trait]
        impl JobsManagerClient for JobsManager {
            async fn startup(&self, node_env: NodeEnv) -> Result<()>;
            async fn get_active_jobs_shutdown_timeout(&self) -> Duration;
            async fn shutdown(&self, force: bool) -> Result<()>;
            async fn list(&self) -> Result<JobsInfo>;
            async fn create(&self, name: &str, config: JobConfig) -> Result<()>;
            async fn start(&self, name: &str) -> Result<()>;
            async fn get_job_shutdown_timeout(&self, name: &str) -> Duration;
            async fn stop(&self, name: &str) -> Result<()>;
            async fn skip(&self, name: &str) -> Result<()>;
            async fn cleanup(&self, name: &str) -> Result<()>;
            async fn info(&self, name: &str) -> Result<JobInfo>;
        }
    }

    struct DummyPal;

    #[async_trait]
    impl BabelPal for DummyPal {
        type BabelServer = UdsServer;
        fn babel_server(&self) -> Self::BabelServer {
            UdsServer
        }

        type Connector = UdsConnector;
        fn connector(&self) -> Self::Connector {
            UdsConnector
        }

        async fn setup_node(&self) -> eyre::Result<()> {
            Ok(())
        }

        async fn set_ram_disks(&self, _ram_disks: Vec<RamdiskConfiguration>) -> eyre::Result<()> {
            Ok(())
        }

        async fn is_ram_disks_set(&self, _ram_disks: Vec<RamdiskConfiguration>) -> Result<bool> {
            Ok(false)
        }
    }

    fn test_client(tmp_root: &Path) -> Result<BabelClient<Channel>> {
        Ok(BabelClient::new(bv_tests_utils::rpc::test_channel(
            tmp_root,
        )))
    }

    struct TestEnv {
        job_runner_path: PathBuf,
        job_runner_lock: JobRunnerLock,
        client: BabelClient<Channel>,
        server: bv_tests_utils::rpc::TestServer,
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        std::fs::create_dir_all(&tmp_root)?;
        let job_runner_path = tmp_root.join("job_runner");
        let babel_cfg_path = tmp_root.join("babel.cfg");
        let job_runner_lock = Arc::new(RwLock::new(None));
        let client = test_client(&tmp_root)?;
        let mut jobs_manager_mock = MockJobsManager::new();
        jobs_manager_mock.expect_startup().returning(|_| Ok(()));
        let babel_service = BabelService::new(
            job_runner_lock.clone(),
            job_runner_path.clone(),
            jobs_manager_mock,
            babel_cfg_path,
            DummyPal,
        )?;
        let server = start_test_server!(
            &tmp_root,
            babel_api::babel::babel_server::BabelServer::new(babel_service)
        );

        Ok(TestEnv {
            job_runner_path,
            job_runner_lock,
            client,
            server,
        })
    }

    fn build_babel_service_with_defaults() -> Result<BabelService<MockJobsManager, DummyPal>> {
        BabelService::new(
            Arc::new(Default::default()),
            Default::default(),
            MockJobsManager::new(),
            Default::default(),
            DummyPal,
        )
    }

    #[tokio::test]
    async fn test_jrpc_json_ok() -> Result<()> {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/")
            .match_header("Content-Type", "application/json")
            .match_header("custom_header", "some value")
            .match_body(mockito::Matcher::Json(json!({
                "id": 0,
                "jsonrpc": "2.0",
                "method": "info_get",
                "params": {"chain": "x"},
            })))
            .with_status(201)
            .with_header("Content-Type", "application/json")
            .with_body(
                json!({
                        "id": 0,
                        "jsonrpc": "2.0",
                        "result": {"info": {"height": 123, "address": "abc"}},
                })
                .to_string(),
            )
            .create();

        let service = build_babel_service_with_defaults()?;
        let output = service
            .run_jrpc(Request::new(JrpcRequest {
                host: server.url(),
                method: "info_get".to_string(),
                params: Some("{\"chain\": \"x\"}".to_string()),
                headers: Some(vec![(
                    "custom_header".to_string(),
                    "some value".to_string(),
                )]),
            }))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(
            output.body,
            r#"{"id":0,"jsonrpc":"2.0","result":{"info":{"address":"abc","height":123}}}"#
        );
        assert_eq!(output.status_code, 201);
        Ok(())
    }

    #[tokio::test]
    async fn test_rest_json_ok() -> Result<()> {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("GET", "/items")
            .match_header("custom_header", "some value")
            .with_header("Content-Type", "application/json")
            .with_body(json!({"result": [1, 2, 3]}).to_string())
            .create();

        let service = build_babel_service_with_defaults()?;
        let output = service
            .run_rest(Request::new(RestRequest {
                url: format!("{}/items", server.url()),
                headers: Some(vec![(
                    "custom_header".to_string(),
                    "some value".to_string(),
                )]),
            }))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(output.body, r#"{"result":[1,2,3]}"#);
        assert_eq!(output.status_code, 200);
        Ok(())
    }

    #[tokio::test]
    async fn test_sh() -> Result<()> {
        let service = build_babel_service_with_defaults()?;

        let output = service
            .run_sh(Request::new(
                r#"echo "make a toast"; >&2 echo "error""#.to_string(),
            ))
            .await?
            .into_inner();
        assert_eq!(output.stderr, "error\n");
        assert_eq!(output.stdout, "make a toast\n");
        assert_eq!(output.exit_code, 0);
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
            utils::file_checksum(&test_env.job_runner_path)
                .await
                .unwrap()
        );
        test_env.server.assert().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_check_job_runner() -> Result<()> {
        let job_runner_bin_path = TempDir::new().unwrap().join("job_runner");
        let job_runner_lock = Arc::new(RwLock::new(None));

        let service = BabelService::new(
            job_runner_lock.clone(),
            job_runner_bin_path,
            MockJobsManager::new(),
            Default::default(),
            DummyPal,
        )?;

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
        let destination_path = temp_dir().join("out.txt");

        let service = build_babel_service_with_defaults()?;

        service
            .render_template(Request::new((
                template_path.clone(),
                destination_path.clone(),
                r#"{"param1": "value1", "param2": 2, "param3": true, "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await?;
        assert_eq!(
            "p1: value1, p2: 2, p3: true; (a)(bb)(ccc)",
            fs::read_to_string(&destination_path).await?
        );

        service
            .render_template(Request::new((
                template_path.clone(),
                destination_path.clone(),
                r#"{"param1": "value1", "param2": 2, "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await?;
        assert_eq!(
            "p1: value1, p2: 2, p3: None; (a)(bb)(ccc)",
            fs::read_to_string(&destination_path).await?
        );

        service
            .render_template(Request::new((
                template_path.clone(),
                destination_path.clone(),
                r#"{"param1": "value1", "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await
            .unwrap_err();

        service
            .render_template(Request::new((
                Path::new("invalid/path").to_path_buf(),
                destination_path.clone(),
                r#"{"param1": "value1", "param2": 2, "vParam": ["a", "bb", "ccc"]}"#.to_string(),
            )))
            .await
            .unwrap_err();

        Ok(())
    }
}
