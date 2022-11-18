use crate::config;
use crate::error;
use babel_api::*;
use serde_json::json;
use std::time::Duration;

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

impl MsgHandler {
    pub fn new(cfg: config::Babel, timeout: Duration) -> Result<Self, error::Error> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { inner: client, cfg })
    }

    pub async fn handle(&self, req: BabelRequest) -> Result<BabelResponse, error::Error> {
        use BabelResponse::*;

        match req {
            BabelRequest::ListCapabilities => Ok(ListCapabilities(self.handle_list_caps())),
            BabelRequest::Ping => Ok(Pong),
            BabelRequest::BlockchainCommand(cmd) => {
                tracing::debug!("Handling BlockchainCommand: `{cmd:?}`");
                self.handle_cmd(cmd).await.map(BlockchainResponse)
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
                let res = babel_api::BlockchainResponse {
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
    use httpmock::prelude::*;
    use std::collections::BTreeMap;

    fn unwrap_blockchain(resp: BabelResponse) -> BlockchainResponse {
        use BabelResponse::*;

        match resp {
            ListCapabilities(_) => panic!("Called `unwrap_blockchain` on `ListCapabilities`"),
            Pong => panic!("Called `unwrap_blockchain` on `Pong`"),
            BabelResponse::BlockchainResponse(resp) => resp,
            Error(_) => panic!("Called `unwrap_blockchain` on `Error`"),
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
            },
            supervisor: supervisor::Config {
                backoff_timeout_ms: 10,
                backoff_base_ms: 1,
                log_buffer_capacity_ln: 1,
                entry_point: vec![],
            },
            monitor: None,
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
        let server = MsgHandler::new(cfg, Duration::from_secs(10)).unwrap();

        let raw_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "raw".to_string(),
        });
        let output = server.handle(raw_cmd).await.unwrap();
        assert_eq!(unwrap_blockchain(output).value, "make a toast\n");

        let json_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "json".to_string(),
        });
        let output = server.handle(json_cmd).await.unwrap();
        assert_eq!(unwrap_blockchain(output).value, "\"make a toast\"");

        let unknown_cmd = BabelRequest::BlockchainCommand(BlockchainCommand {
            name: "unknown".to_string(),
        });
        let output = server.handle(unknown_cmd).await;
        assert_eq!(
            output.unwrap_err().to_string(),
            "Method `unknown` not found"
        );
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
            },
            supervisor: supervisor::Config {
                backoff_timeout_ms: 10,
                backoff_base_ms: 1,
                log_buffer_capacity_ln: 1,
                entry_point: vec![],
            },
            monitor: None,
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
        let server = MsgHandler::new(cfg, Duration::from_secs(1)).unwrap();
        let output = server.handle(json_cmd).await.unwrap();

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
            },
            supervisor: supervisor::Config {
                backoff_timeout_ms: 10,
                backoff_base_ms: 1,
                log_buffer_capacity_ln: 1,
                entry_point: vec![],
            },
            monitor: None,
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
        let server = MsgHandler::new(cfg, Duration::from_secs(1)).unwrap();
        let output = server.handle(json_cmd).await.unwrap();

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
            },
            supervisor: supervisor::Config {
                backoff_timeout_ms: 10,
                backoff_base_ms: 1,
                log_buffer_capacity_ln: 1,
                entry_point: vec![],
            },
            monitor: None,
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
        let server = MsgHandler::new(cfg, Duration::from_secs(1)).unwrap();
        let output = server.handle(height_cmd).await.unwrap();

        mock.assert();
        assert_eq!(unwrap_blockchain(output).value, "123");
    }
}
