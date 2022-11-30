use crate::config;
use crate::error;
use babel_api::*;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::fs::{self, DirBuilder, File};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Mutex};

const WILDCARD_KEY_NAME: &str = "*";

pub struct MsgHandler {
    inner: reqwest::Client,
    cfg: config::Babel,
    logs_rx: Mutex<broadcast::Receiver<String>>,
}

impl std::ops::Deref for MsgHandler {
    type Target = reqwest::Client;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl MsgHandler {
    pub fn new(
        cfg: config::Babel,
        timeout: Duration,
        logs_rx: broadcast::Receiver<String>,
    ) -> Result<Self, error::Error> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            inner: client,
            cfg,
            logs_rx: Mutex::new(logs_rx),
        })
    }

    pub async fn handle(&self, req: BabelRequest) -> Result<BabelResponse, error::Error> {
        use BabelResponse::*;

        match req {
            BabelRequest::ListCapabilities => Ok(ListCapabilities(self.handle_list_caps())),
            BabelRequest::Ping => Ok(Pong),
            BabelRequest::Logs => Ok(Logs(self.handle_logs().await)),
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

    /// List logs from blockchain entry_points.
    async fn handle_logs(&self) -> Vec<String> {
        let mut logs = Vec::default();
        let mut rx = self.logs_rx.lock().await;
        loop {
            match rx.try_recv() {
                Ok(log) => logs.push(log),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        logs
    }

    async fn handle_download_keys(&self) -> Result<Vec<BlockchainKey>, error::Error> {
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

    async fn handle_upload_keys(
        &self,
        keys: Vec<BlockchainKey>,
    ) -> Result<BlockchainResponse, error::Error> {
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

    async fn handle_cmd(&self, cmd: BlockchainCommand) -> Result<BlockchainResponse, error::Error> {
        use config::Method::*;

        let method = self
            .cfg
            .methods
            .get(&cmd.name)
            .ok_or_else(|| crate::error::Error::unknown_method(cmd.name))?;
        tracing::debug!("Chosen method is {method:?}");

        match method {
            Jrpc {
                method, response, ..
            } => self.handle_jrpc(method, response).await,
            Rest {
                method, response, ..
            } => self.handle_rest(method, response).await,
            Sh { body, response, .. } => Self::handle_sh(body, response).await,
        }
    }

    async fn handle_jrpc(
        &self,
        method: &str,
        resp_config: &config::JrpcResponse,
    ) -> Result<BlockchainResponse, error::Error> {
        let url = self
            .cfg
            .config
            .api_host
            .as_deref()
            .ok_or_else(|| error::Error::no_host(method))?;
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
        resp_config: &config::RestResponse,
    ) -> Result<BlockchainResponse, error::Error> {
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

        let text = self.post(&url).send().await?.text().await?;
        let value = match &resp_config.field {
            Some(field) => gjson::get(&text, field).to_string(),
            None => text,
        };
        Ok(BlockchainResponse { value })
    }

    async fn handle_sh(
        command: &str,
        response_config: &config::ShResponse,
    ) -> Result<BlockchainResponse, error::Error> {
        use config::MethodResponseFormat::*;

        let args = vec!["-c", command];
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
    use crate::config::{
        Babel, Config, JrpcResponse, Method, MethodResponseFormat, RestResponse, ShResponse,
    };
    use crate::supervisor;
    use assert_fs::TempDir;
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
    async fn test_logs() {
        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                babel_version: "".to_string(),
                node_version: "".to_string(),
                protocol: "".to_string(),
                node_type: "".to_string(),
                description: None,
                api_host: None,
                ports: vec![],
                data_directory_mount_point: "".to_string(),
            },
            supervisor: supervisor::Config {
                backoff_timeout_ms: 10,
                backoff_base_ms: 1,
                log_buffer_capacity_ln: 4,
                entry_point: vec![],
            },
            keys: None,
            methods: Default::default(),
        };
        let (tx, logs_rx) = broadcast::channel(16);
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(10), logs_rx).unwrap();
        tx.send("log1".to_string()).expect("failed to send log");
        tx.send("log2".to_string()).expect("failed to send log");
        tx.send("log3".to_string()).expect("failed to send log");

        if let BabelResponse::Logs(logs) = msg_handler
            .handle(BabelRequest::Logs)
            .await
            .expect("failed to handle logs request")
        {
            assert_eq!(vec!["log1", "log2", "log3"], logs)
        } else {
            panic!("invalid logs response")
        }
    }

    #[tokio::test]
    async fn test_sh() {
        let cfg = Babel {
            export: None,
            env: None,
            config: Config {
                babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: None,
                ports: vec![],
            },
            supervisor: supervisor::Config::default(),
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
        let (_, logs_rx) = broadcast::channel(1);
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(10), logs_rx).unwrap();

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
        });
        let output = msg_handler.handle(raw_cmd).await.unwrap();
        assert_eq!(unwrap_blockchain(output).value, "make a toast\n");

        let json_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "json".to_string(),
        });
        let output = msg_handler.handle(json_cmd).await.unwrap();
        assert_eq!(unwrap_blockchain(output).value, "\"make a toast\"");

        let unknown_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "unknown".to_string(),
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
                babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: None,
                ports: vec![],
            },
            supervisor: supervisor::Config::default(),
            keys: Some(HashMap::from([
                ("first".to_string(), format!("{tmp_dir_str}/first/key")),
                ("second".to_string(), format!("{tmp_dir_str}/second/key")),
                ("third".to_string(), format!("{tmp_dir_str}/third/key")),
            ])),
            methods: BTreeMap::new(),
        };
        let (_, logs_rx) = broadcast::channel(1);
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(10), logs_rx).unwrap();

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
                babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: None,
                ports: vec![],
            },
            supervisor: supervisor::Config::default(),
            keys: Some(HashMap::from([(
                WILDCARD_KEY_NAME.to_string(),
                format!("{tmp_dir_str}/star/"),
            )])),
            methods: BTreeMap::new(),
        };
        let (_, logs_rx) = broadcast::channel(1);
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(10), logs_rx).unwrap();

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
                babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: Some(format!("http://{}", server.address())),
                ports: vec![],
            },
            supervisor: supervisor::Config::default(),
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
        });
        let (_, logs_rx) = broadcast::channel(1);
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(1), logs_rx).unwrap();
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
                babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: Some(format!("http://{}", server.address())),
                ports: vec![],
            },
            supervisor: supervisor::Config::default(),
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
        });
        let (_, logs_rx) = broadcast::channel(1);
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(1), logs_rx).unwrap();
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
                babel_version: "0.1.0".to_string(),
                node_version: "1.51.3".to_string(),
                protocol: "helium".to_string(),
                node_type: "".to_string(),
                data_directory_mount_point: "/tmp".to_string(),
                description: None,
                api_host: Some(format!("http://{}", server.address())),
                ports: vec![],
            },
            supervisor: supervisor::Config::default(),
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
        });
        let (_, logs_rx) = broadcast::channel(1);
        let msg_handler = MsgHandler::new(cfg, Duration::from_secs(1), logs_rx).unwrap();
        let output = msg_handler.handle(height_cmd).await.unwrap();

        mock.assert();
        assert_eq!(unwrap_blockchain(output).value, "123");
    }
}
