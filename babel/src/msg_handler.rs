#[cfg(target_os = "linux")]
use crate::vsock::Handler;
use crate::{error, utils, Result};
#[cfg(target_os = "linux")]
use async_trait::async_trait;
use babel_api::config;
use babel_api::*;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::fs::{self, DirBuilder, File};
use tokio::io::AsyncWriteExt;

const WILDCARD_KEY_NAME: &str = "*";

pub struct MsgHandler {
    inner: reqwest::Client,
    cfg: config::Babel,
}

impl std::ops::Deref for MsgHandler {
    type Target = reqwest::Client;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl Handler for MsgHandler {
    async fn handle(&self, message: &str) -> eyre::Result<Vec<u8>> {
        use eyre::Context;

        let request: babel_api::BabelRequest = serde_json::from_str(message)
            .wrap_err(format!("Could not parse request as json: '{message}'"))?;

        let resp = match self.handle(request).await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!("Failed to handle message: {e}");
                babel_api::BabelResponse::Error(e.to_string())
            }
        };

        Ok(serde_json::to_vec(&resp)?)
    }
}

impl MsgHandler {
    pub fn new(cfg: config::Babel, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { inner: client, cfg })
    }

    pub async fn handle(&self, req: BabelRequest) -> Result<BabelResponse> {
        use BabelResponse::*;

        match req {
            BabelRequest::ListCapabilities => Ok(ListCapabilities(self.handle_list_caps())),
            BabelRequest::Ping => Ok(Pong),
            BabelRequest::BlockchainCommand(cmd) => {
                tracing::debug!("Handling BlockchainCommand: `{cmd:?}`");
                self.handle_cmd(cmd).await.map(BlockchainResponse)
            }
            BabelRequest::DownloadKeys => {
                tracing::debug!("Downloading keys");
                Ok(Keys(self.handle_download_keys().await?))
            }
            BabelRequest::UploadKeys(keys) => {
                tracing::debug!("Uploading keys: `{keys:?}`");
                self.handle_upload_keys(keys).await.map(BlockchainResponse)
            }
        }
    }

    /// List the capabilities that the current blockchain node supports.
    fn handle_list_caps(&self) -> Vec<String> {
        self.cfg
            .methods
            .keys()
            .map(|method| method.to_string())
            .collect()
    }

    async fn handle_download_keys(&self) -> Result<Vec<BlockchainKey>> {
        let config = self
            .cfg
            .keys
            .as_ref()
            .ok_or_else(|| error::Error::keys("No `keys` section found in config"))?;

        let mut results = vec![];

        for (name, location) in config.iter() {
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

        Ok(results)
    }

    async fn handle_upload_keys(&self, keys: Vec<BlockchainKey>) -> Result<BlockchainResponse> {
        if keys.is_empty() {
            return Err(error::Error::keys("No keys provided"));
        }

        let config = self
            .cfg
            .keys
            .as_ref()
            .ok_or_else(|| error::Error::keys("No `keys` section found in config"))?;

        if !config.contains_key(WILDCARD_KEY_NAME) {
            for key in &keys {
                let name = &key.name;
                if !config.contains_key(name) {
                    return Err(error::Error::keys(format!(
                        "Key `{name}` not found in `keys` config"
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

        let resp = BlockchainResponse {
            value: results.join("\n"),
        };

        Ok(resp)
    }

    async fn handle_cmd(&self, cmd: BlockchainCommand) -> Result<BlockchainResponse> {
        use config::Method::*;

        let method = self
            .cfg
            .methods
            .get(&cmd.name)
            .ok_or_else(|| crate::error::Error::unknown_method(cmd.name))?;
        tracing::debug!("Chosen method is {method:?}");

        match &method {
            Jrpc {
                method, response, ..
            } => self.handle_jrpc(method, cmd.params, response).await,
            Rest {
                method, response, ..
            } => self.handle_rest(method, cmd.params, response).await,
            Sh { body, response, .. } => Self::handle_sh(body, cmd.params, response).await,
        }
    }

    async fn handle_jrpc(
        &self,
        method: &str,
        params: BlockchainParams,
        resp_config: &config::JrpcResponse,
    ) -> Result<BlockchainResponse> {
        let url = self
            .cfg
            .config
            .api_host
            .as_deref()
            .ok_or_else(|| error::Error::no_host(method))?;
        let params = params.into_iter().map(|(k, v)| (k, v.join(","))).collect();
        let method = utils::render(method, &params);
        let text: String = self
            .post(url)
            .json(&json!({ "jsonrpc": "2.0", "id": 0, "method": method }))
            .send()
            .await?
            .text()
            .await?;
        let value = if let Some(field) = &resp_config.field {
            tracing::debug!("Retrieving field `{field}` from the body `{text}`");
            gjson::get(&text, field).to_string()
        } else {
            text
        };
        let resp = BlockchainResponse { value };
        Ok(resp)
    }

    async fn handle_rest(
        &self,
        method: &str,
        params: BlockchainParams,
        resp_config: &config::RestResponse,
    ) -> Result<BlockchainResponse> {
        let host = self
            .cfg
            .config
            .api_host
            .as_ref()
            .ok_or_else(|| error::Error::no_host(method))?;
        let url = format!(
            "{}/{}",
            host.trim_end_matches('/'),
            method.trim_start_matches('/')
        );
        let params = params.into_iter().map(|(k, v)| (k, v.join(","))).collect();
        let url = utils::render(&url, &params);
        let text = self.post(url.as_str()).send().await?.text().await?;
        let value = match &resp_config.field {
            Some(field) => gjson::get(&text, field).to_string(),
            None => text,
        };
        Ok(BlockchainResponse { value })
    }

    async fn handle_sh(
        command: &str,
        params: BlockchainParams,
        response_config: &config::ShResponse,
    ) -> Result<BlockchainResponse> {
        use config::MethodResponseFormat::*;

        let command = &mut command.to_string();

        // For sh we need to sanitize each param, then join them.
        let params = params
            .into_iter()
            .map(|(k, v)| Ok((k, utils::sanitize_param(&v)?)))
            .collect::<Result<_>>()?;
        let command = utils::render(command, &params);

        let args = vec!["-c", &command];
        let output = tokio::process::Command::new("sh")
            .args(args)
            .output()
            .await?;

        if !output.status.success() {
            return Err(error::Error::command(command, output));
        }

        match response_config.format {
            Json => {
                let content: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                let res = BlockchainResponse {
                    value: serde_json::to_string(&content)?,
                };
                Ok(res)
            }
            Raw => {
                let content = String::from_utf8_lossy(&output.stdout).to_string();
                Ok(content.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use babel_api::config::{
        Babel, Config, JrpcResponse, Method, MethodResponseFormat, Requirements, RestResponse,
        ShResponse, SupervisorConfig,
    };
    use httpmock::prelude::*;
    use std::collections::{BTreeMap, HashMap};

    fn unwrap_blockchain(resp: BabelResponse) -> BlockchainResponse {
        if let BabelResponse::BlockchainResponse(inner) = resp {
            inner
        } else {
            panic!("Expected `BlockchainResponse`, but received `{resp:?}`")
        }
    }

    fn unwrap_keys(resp: BabelResponse) -> Vec<BlockchainKey> {
        if let BabelResponse::Keys(inner) = resp {
            inner
        } else {
            panic!("Expected `Keys`, but received `{resp:?}`")
        }
    }

    #[tokio::test]
    async fn test_sh() {
        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                min_babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: None,
                ports: vec![],
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 1,
                disk_size_gb: 1,
            },
            supervisor: SupervisorConfig::default(),
            keys: None,
            methods: BTreeMap::from([
                (
                    "raw".to_string(),
                    Method::Sh {
                        name: "raw".to_string(),
                        body: "echo make a toast".to_string(),
                        response: ShResponse {
                            status: 101,
                            format: MethodResponseFormat::Raw,
                        },
                    },
                ),
                (
                    "json".to_string(),
                    Method::Sh {
                        name: "json".to_string(),
                        body: "echo \\\"make a toast\\\"".to_string(),
                        response: ShResponse {
                            status: 102,
                            format: MethodResponseFormat::Json,
                        },
                    },
                ),
            ]),
        };
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(10)).unwrap();

        let caps = msg_handler
            .handle(BabelRequest::ListCapabilities)
            .await
            .unwrap();
        match caps {
            BabelResponse::ListCapabilities(caps) => {
                assert_eq!(caps[0], "json");
                assert_eq!(caps[1], "raw");
            }
            _ => panic!("expected ListCapabilities"),
        };

        let raw_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "raw".to_string(),
            params: HashMap::new(),
        });
        let output = msg_handler.handle(raw_cmd).await.unwrap();
        assert_eq!(unwrap_blockchain(output).value, "make a toast\n");

        let json_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "json".to_string(),
            params: HashMap::new(),
        });
        let output = msg_handler.handle(json_cmd).await.unwrap();
        assert_eq!(unwrap_blockchain(output).value, "\"make a toast\"");

        let unknown_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "unknown".to_string(),
            params: HashMap::new(),
        });
        let output = msg_handler.handle(unknown_cmd).await;
        assert_eq!(
            output.unwrap_err().to_string(),
            "Method `unknown` not found"
        );
    }

    #[tokio::test]
    async fn test_upload_download_keys() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_dir_str = format!("{}", tmp_dir.to_string_lossy());

        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                min_babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: None,
                ports: vec![],
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 1,
                disk_size_gb: 1,
            },
            supervisor: SupervisorConfig::default(),
            keys: Some(HashMap::from([
                ("first".to_string(), format!("{tmp_dir_str}/first/key")),
                ("second".to_string(), format!("{tmp_dir_str}/second/key")),
                ("third".to_string(), format!("{tmp_dir_str}/third/key")),
            ])),
            methods: BTreeMap::new(),
        };
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(10)).unwrap();

        println!("no files uploaded yet");
        let output = msg_handler
            .handle(BabelRequest::DownloadKeys)
            .await
            .unwrap();
        let keys = unwrap_keys(output);
        assert_eq!(keys.len(), 0);

        println!("upload bad keys");
        let err = msg_handler
            .handle(BabelRequest::UploadKeys(vec![]))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Keys management error: No keys provided");
        let err = msg_handler
            .handle(BabelRequest::UploadKeys(vec![BlockchainKey {
                name: "unknown".to_string(),
                content: vec![],
            }]))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Keys management error: Key `unknown` not found in `keys` config"
        );

        println!("upload good keys");
        let output = msg_handler
            .handle(BabelRequest::UploadKeys(vec![
                BlockchainKey {
                    name: "second".to_string(),
                    content: b"123".to_vec(),
                },
                BlockchainKey {
                    name: "third".to_string(),
                    content: b"abcd".to_vec(),
                },
            ]))
            .await
            .unwrap();
        assert_eq!(
            unwrap_blockchain(output).value,
            format!(
                "Done writing 3 bytes of key `second` into `{tmp_dir_str}/second/key`\n\
                 Done writing 4 bytes of key `third` into `{tmp_dir_str}/third/key`"
            )
        );

        println!("download uploaded keys");
        let output = msg_handler
            .handle(BabelRequest::DownloadKeys)
            .await
            .unwrap();
        let mut keys = unwrap_keys(output);
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
    }

    #[tokio::test]
    async fn test_upload_download_keys_with_star() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_dir_str = format!("{}", tmp_dir.to_string_lossy());

        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                min_babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: None,
                ports: vec![],
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 1,
                disk_size_gb: 1,
            },
            supervisor: SupervisorConfig::default(),
            keys: Some(HashMap::from([(
                WILDCARD_KEY_NAME.to_string(),
                format!("{tmp_dir_str}/star/"),
            )])),
            methods: BTreeMap::new(),
        };
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(10)).unwrap();

        println!("upload unknown keys");
        let output = msg_handler
            .handle(BabelRequest::UploadKeys(vec![BlockchainKey {
                name: "unknown".to_string(),
                content: b"12345".to_vec(),
            }]))
            .await
            .unwrap();
        assert_eq!(
            unwrap_blockchain(output).value,
            format!("Done writing 5 bytes of key `unknown` into `{tmp_dir_str}/star/unknown`")
        );

        println!("files in star dir should not be downloading");
        let output = msg_handler
            .handle(BabelRequest::DownloadKeys)
            .await
            .unwrap();
        let keys = unwrap_keys(output);
        assert_eq!(keys.len(), 0);
    }

    #[tokio::test]
    async fn test_rest_json_ok() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST).path("/items");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"result": [1, 2, 3]}));
        });

        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                min_babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: Some(format!("http://{}", server.address())),
                ports: vec![],
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 1,
                disk_size_gb: 1,
            },
            supervisor: SupervisorConfig::default(),
            keys: None,
            methods: BTreeMap::from([(
                "json items".to_string(),
                Method::Rest {
                    name: "json items".to_string(),
                    method: "items".to_string(),
                    response: RestResponse {
                        status: 101,
                        field: Some("result".to_string()),
                        format: MethodResponseFormat::Json,
                    },
                },
            )]),
        };

        let json_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "json items".to_string(),
            params: HashMap::new(),
        });
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(1)).unwrap();
        let output = msg_handler.handle(json_cmd).await.unwrap();

        mock.assert();
        assert_eq!(unwrap_blockchain(output).value, "[1,2,3]");
    }

    #[tokio::test]
    async fn test_rest_json_full_response_ok() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST).path("/items");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"result": [1, 2, 3]}));
        });

        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                min_babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: Some(format!("http://{}", server.address())),
                ports: vec![],
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 1,
                disk_size_gb: 1,
            },
            supervisor: SupervisorConfig::default(),
            keys: None,
            methods: BTreeMap::from([(
                "json items".to_string(),
                Method::Rest {
                    name: "json items".to_string(),
                    method: "items".to_string(),
                    response: RestResponse {
                        status: 101,
                        field: None,
                        format: MethodResponseFormat::Json,
                    },
                },
            )]),
        };

        let json_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "json items".to_string(),
            params: HashMap::new(),
        });
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(1)).unwrap();
        let output = msg_handler.handle(json_cmd).await.unwrap();

        mock.assert();
        assert_eq!(unwrap_blockchain(output).value, "{\"result\":[1,2,3]}");
    }

    #[tokio::test]
    async fn test_jrpc_json_ok() {
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

        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                min_babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: Some(format!("http://{}", server.address())),
                ports: vec![],
            },
            requirements: Requirements {
                vcpu_count: 1,
                mem_size_mb: 1,
                disk_size_gb: 1,
            },
            supervisor: SupervisorConfig::default(),
            keys: None,
            methods: BTreeMap::from([(
                "get height".to_string(),
                Method::Jrpc {
                    name: "get height".to_string(),
                    method: "info_get".to_string(),
                    response: JrpcResponse {
                        code: 101,
                        field: Some("result.info.height".to_string()),
                    },
                },
            )]),
        };

        let height_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "get height".to_string(),
            params: HashMap::new(),
        });
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(1)).unwrap();
        let output = msg_handler.handle(height_cmd).await.unwrap();

        mock.assert();
        assert_eq!(unwrap_blockchain(output).value, "123");
    }
}
