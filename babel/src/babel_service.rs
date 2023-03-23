use crate::ufw_wrapper::apply_firewall_config;
use crate::utils;
use async_trait::async_trait;
use babel_api::config::{JobConfig, JobStatus};
use babel_api::BlockchainKey;
use eyre::{bail, Result};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::{path::Path, time::Duration};
use tokio::sync::RwLock;
use tokio::{
    fs,
    fs::{DirBuilder, File},
    io::AsyncWriteExt,
};
use tonic::{Request, Response, Status, Streaming};

const WILDCARD_KEY_NAME: &str = "*";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DATA_DRIVE_PATH: &str = "/dev/vdb";

/// Lock used to avoid reading job runner binary while it is modified.
/// It stores CRC32 checksum of the binary file.
pub type JobRunnerLock = Arc<RwLock<Option<u32>>>;

pub struct BabelService {
    inner: reqwest::Client,
    job_runner_lock: JobRunnerLock,
    job_runner_bin_path: PathBuf,
}

impl std::ops::Deref for BabelService {
    type Target = reqwest::Client;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[async_trait]
impl babel_api::babel_server::Babel for BabelService {
    async fn setup_firewall(
        &self,
        request: Request<babel_api::config::firewall::Config>,
    ) -> Result<Response<()>, Status> {
        apply_firewall_config(request.into_inner())
            .await
            .map_err(|err| {
                Status::internal(format!("failed to apply firewall config with: {err}"))
            })?;
        Ok(Response::new(()))
    }

    async fn mount_data_directory(&self, request: Request<String>) -> Result<Response<()>, Status> {
        let data_directory_mount_point = request.into_inner();
        // We assume that root drive will become /dev/vda, and data drive will become /dev/vdb inside VM
        // However, this can be a wrong assumption ¯\_(ツ)_/¯:
        // https://github.com/firecracker-microvm/firecracker-containerd/blob/main/docs/design-approaches.md#block-devices
        let out = utils::mount_drive(DATA_DRIVE_PATH, &data_directory_mount_point)
            .await
            .map_err(|err| Status::internal(format!("failed to mount {DATA_DRIVE_PATH} into {data_directory_mount_point} with: {err}")))?;
        match out.status.code() {
            Some(0) => Ok(Response::new(())),
            Some(32) if String::from_utf8_lossy(&out.stderr).contains("already mounted") => {
                Err(Status::already_exists(format!(
                    "drive {DATA_DRIVE_PATH} already mounted into {data_directory_mount_point} "
                )))
            }
            _ => Err(Status::internal(format!(
                "failed to mount {DATA_DRIVE_PATH} into {data_directory_mount_point} with: {out:?}"
            ))),
        }
    }

    async fn download_keys(
        &self,
        request: Request<babel_api::config::KeysConfig>,
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
        request: Request<(babel_api::config::KeysConfig, Vec<BlockchainKey>)>,
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
    ) -> Result<Response<babel_api::BinaryStatus>, Status> {
        let expected_checksum = request.into_inner();
        let job_runner_status = match *self.job_runner_lock.read().await {
            Some(checksum) if checksum == expected_checksum => babel_api::BinaryStatus::Ok,
            Some(_) => babel_api::BinaryStatus::ChecksumMismatch,
            None => babel_api::BinaryStatus::Missing,
        };
        Ok(Response::new(job_runner_status))
    }

    async fn upload_job_runner(
        &self,
        request: Request<Streaming<babel_api::Binary>>,
    ) -> Result<Response<()>, Status> {
        let mut stream = request.into_inner();
        // block using job runner binary
        let mut lock = self.job_runner_lock.write().await;
        let checksum = utils::save_bin_stream(&self.job_runner_bin_path, &mut stream)
            .await
            .map_err(|e| Status::internal(format!("upload_job_runner failed with {e}")))?;
        lock.replace(checksum);
        Ok(Response::new(()))
    }

    async fn start_job(
        &self,
        _request: Request<(String, JobConfig)>,
    ) -> Result<Response<()>, Status> {
        unimplemented!();
    }

    async fn stop_job(&self, _request: Request<String>) -> Result<Response<()>, Status> {
        unimplemented!();
    }

    async fn job_status(&self, _request: Request<String>) -> Result<Response<JobStatus>, Status> {
        unimplemented!();
    }

    async fn blockchain_jrpc(
        &self,
        request: Request<(String, String, babel_api::config::JrpcResponse)>,
    ) -> Result<Response<String>, Status> {
        Ok(Response::new(
            self.handle_jrpc(request).await.map_err(to_blockchain_err)?,
        ))
    }

    async fn blockchain_rest(
        &self,
        request: Request<(String, babel_api::config::RestResponse)>,
    ) -> Result<Response<String>, Status> {
        Ok(Response::new(
            self.handle_rest(request).await.map_err(to_blockchain_err)?,
        ))
    }

    async fn blockchain_sh(
        &self,
        request: Request<(String, babel_api::config::ShResponse)>,
    ) -> Result<Response<String>, Status> {
        Ok(Response::new(
            self.handle_sh(request).await.map_err(to_blockchain_err)?,
        ))
    }
}

fn to_blockchain_err(err: eyre::Error) -> Status {
    Status::internal(err.to_string())
}

impl BabelService {
    pub fn new(job_runner_lock: JobRunnerLock, job_runner_bin_path: PathBuf) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            inner: client,
            job_runner_lock,
            job_runner_bin_path,
        })
    }

    async fn handle_jrpc(
        &self,
        request: Request<(String, String, babel_api::config::JrpcResponse)>,
    ) -> Result<String> {
        let (host, method, response) = request.into_inner();
        let text: String = self
            .post(host)
            .json(&json!({ "jsonrpc": "2.0", "id": 0, "method": method }))
            .send()
            .await?
            .text()
            .await?;
        let value = if let Some(field) = &response.field {
            tracing::debug!("Retrieving field `{field}` from the body `{text}`");
            gjson::get(&text, field).to_string()
        } else {
            text
        };
        Ok(value)
    }

    async fn handle_rest(
        &self,
        request: Request<(String, babel_api::config::RestResponse)>,
    ) -> Result<String> {
        let (url, response) = request.into_inner();
        let text = self.get(url).send().await?.text().await?;
        let value = match &response.field {
            Some(field) => gjson::get(&text, field).to_string(),
            None => text,
        };
        Ok(value)
    }

    async fn handle_sh(
        &self,
        request: Request<(String, babel_api::config::ShResponse)>,
    ) -> Result<String> {
        use babel_api::config::MethodResponseFormat::*;

        let (body, response) = request.into_inner();
        let args = vec!["-c", &body];
        let output = tokio::process::Command::new("sh")
            .args(args)
            .output()
            .await?;

        if !output.status.success() {
            bail!("Failed to run command `{body}`, got output `{output:?}`")
        }

        match response.format {
            Json => {
                let content: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(serde_json::to_string(&content)?)
            }
            Raw => Ok(String::from_utf8_lossy(&output.stdout).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use babel_api::babel_client::BabelClient;
    use babel_api::babel_server::Babel;
    use babel_api::config::{JrpcResponse, MethodResponseFormat, RestResponse, ShResponse};
    use httpmock::prelude::*;
    use std::collections::HashMap;
    use tokio::net::UnixStream;
    use tokio_stream::wrappers::UnixListenerStream;
    use tonic::transport::{Channel, Endpoint, Server, Uri};

    async fn babel_server(
        job_runner_lock: JobRunnerLock,
        job_runner_bin_path: PathBuf,
        uds_stream: UnixListenerStream,
    ) -> Result<()> {
        let babel_service = BabelService::new(job_runner_lock, job_runner_bin_path)?;
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(babel_api::babel_server::BabelServer::new(babel_service))
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
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        std::fs::create_dir_all(&tmp_root)?;
        let path = tmp_root.join("job_runner");
        let lock = Arc::new(RwLock::new(None));
        let client = test_client(&tmp_root)?;
        let uds_stream = UnixListenerStream::new(tokio::net::UnixListener::bind(
            tmp_root.join("test_socket"),
        )?);
        let job_runner_lock = lock.clone();
        let job_runner_bin_path = path.clone();
        tokio::spawn(async move { babel_server(lock, path, uds_stream).await });

        Ok(TestEnv {
            job_runner_bin_path,
            job_runner_lock,
            client,
        })
    }

    fn build_babel_service_with_defaults() -> Result<BabelService> {
        BabelService::new(Arc::new(Default::default()), Default::default())
    }

    #[tokio::test]
    async fn test_upload_download_keys() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_dir_str = format!("{}", tmp_dir.to_string_lossy());

        let service = build_babel_service_with_defaults()?;
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
        let service = build_babel_service_with_defaults()?;

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

        let service = build_babel_service_with_defaults()?;
        let output = service
            .blockchain_jrpc(Request::new((
                format!("http://{}", server.address()),
                "info_get".to_string(),
                JrpcResponse {
                    code: 101,
                    field: Some("result.info.height".to_string()),
                },
            )))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(output, "123");
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

        let service = build_babel_service_with_defaults()?;
        let output = service
            .blockchain_rest(Request::new((
                format!("http://{}/items", server.address()),
                RestResponse {
                    status: 101,
                    field: Some("result".to_string()),
                    format: MethodResponseFormat::Json,
                },
            )))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(output, "[1,2,3]");
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

        let service = build_babel_service_with_defaults()?;
        let output = service
            .blockchain_rest(Request::new((
                format!("http://{}/items", server.address()),
                RestResponse {
                    status: 101,
                    field: None,
                    format: MethodResponseFormat::Json,
                },
            )))
            .await?
            .into_inner();

        mock.assert();
        assert_eq!(output, "{\"result\":[1,2,3]}");
        Ok(())
    }

    #[tokio::test]
    async fn test_sh() -> Result<()> {
        let service = build_babel_service_with_defaults()?;

        let output = service
            .blockchain_sh(Request::new((
                "echo \\\"make a toast\\\"".to_string(),
                ShResponse {
                    status: 102,
                    format: MethodResponseFormat::Json,
                },
            )))
            .await?
            .into_inner();
        assert_eq!(output, "\"make a toast\"");
        Ok(())
    }

    #[tokio::test]
    async fn test_upload_job_runner() -> Result<()> {
        let mut test_env = setup_test_env()?;

        let incomplete_runner_bin = vec![
            babel_api::Binary::Bin(vec![1, 2, 3, 4, 6, 7, 8, 9, 10]),
            babel_api::Binary::Bin(vec![11, 12, 13, 14, 16, 17, 18, 19, 20]),
            babel_api::Binary::Bin(vec![21, 22, 23, 24, 26, 27, 28, 29, 30]),
        ];

        test_env
            .client
            .upload_job_runner(tokio_stream::iter(incomplete_runner_bin.clone()))
            .await
            .unwrap_err();
        assert!(test_env.job_runner_lock.read().await.is_none());

        let mut invalid_runner_bin = incomplete_runner_bin.clone();
        invalid_runner_bin.push(babel_api::Binary::Checksum(123));
        test_env
            .client
            .upload_job_runner(tokio_stream::iter(invalid_runner_bin))
            .await
            .unwrap_err();
        assert!(test_env.job_runner_lock.read().await.is_none());

        let mut runner_bin = incomplete_runner_bin.clone();
        runner_bin.push(babel_api::Binary::Checksum(4135829304));
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

        let service = BabelService::new(job_runner_lock.clone(), job_runner_bin_path)?;

        assert_eq!(
            babel_api::BinaryStatus::Missing,
            service
                .check_job_runner(Request::new(123))
                .await?
                .into_inner()
        );

        job_runner_lock.write().await.replace(321);
        assert_eq!(
            babel_api::BinaryStatus::ChecksumMismatch,
            service
                .check_job_runner(Request::new(123))
                .await?
                .into_inner()
        );
        assert_eq!(
            babel_api::BinaryStatus::Ok,
            service
                .check_job_runner(Request::new(321))
                .await?
                .into_inner()
        );
        Ok(())
    }
}
