#[cfg(target_os = "linux")]
use crate::vsock::Handler;
use crate::{error, Result};
#[cfg(target_os = "linux")]
use async_trait::async_trait;
use babel_api::config;
use babel_api::*;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::fs::{self, DirBuilder, File};
use tokio::io::AsyncWriteExt;

const WILDCARD_KEY_NAME: &str = "*";

pub struct MsgHandler {
    inner: reqwest::Client,
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
    pub fn new(timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { inner: client })
    }

    pub async fn handle(&self, req: BabelRequest) -> Result<BabelResponse> {
        use BabelResponse::*;
        tracing::debug!("Handling babel request: `{req:?}`");

        match req {
            BabelRequest::Ping => Ok(Pong),
            BabelRequest::BlockchainJrpc {
                host,
                method,
                response,
            } => Ok(self
                .handle_jrpc(&host, &method, &response)
                .await
                .map(BlockchainResponse)?),
            BabelRequest::BlockchainRest { url, response, .. } => Ok(self
                .handle_rest(&url, &response)
                .await
                .map(BlockchainResponse)?),
            BabelRequest::BlockchainSh { body, response, .. } => {
                Ok(Self::handle_sh(&body, &response)
                    .await
                    .map(BlockchainResponse)?)
            }
            BabelRequest::DownloadKeys(cfg) => Ok(Keys(self.handle_download_keys(cfg).await?)),
            BabelRequest::UploadKeys((cfg, keys)) => self
                .handle_upload_keys(cfg, keys)
                .await
                .map(BlockchainResponse),
        }
    }

    async fn handle_download_keys(
        &self,
        config: HashMap<String, String>,
    ) -> Result<Vec<BlockchainKey>> {
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
        config: HashMap<String, String>,
        keys: Vec<BlockchainKey>,
    ) -> Result<BlockchainResponse> {
        if keys.is_empty() {
            return Err(error::Error::keys("No keys provided"));
        }

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

    async fn handle_jrpc(
        &self,
        host: &str,
        method: &str,
        resp_config: &config::JrpcResponse,
    ) -> Result<BlockchainResponse> {
        let text: String = self
            .post(host)
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
    ) -> Result<BlockchainResponse> {
        let text = self.post(method).send().await?.text().await?;
        let value = match &resp_config.field {
            Some(field) => gjson::get(&text, field).to_string(),
            None => text,
        };
        Ok(BlockchainResponse { value })
    }

    async fn handle_sh(
        command: &str,
        response_config: &config::ShResponse,
    ) -> Result<BlockchainResponse> {
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
    use assert_fs::TempDir;
    use babel_api::config::{JrpcResponse, MethodResponseFormat, RestResponse, ShResponse};
    use httpmock::prelude::*;
    use std::collections::HashMap;

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
        let msg_handler = MsgHandler::new(Duration::from_secs(10)).unwrap();

        let json_cmd = BabelRequest::BlockchainSh {
            body: "echo \\\"make a toast\\\"".to_string(),
            response: ShResponse {
                status: 102,
                format: MethodResponseFormat::Json,
            },
        };
        let output = msg_handler.handle(json_cmd).await.unwrap();
        assert_eq!(unwrap_blockchain(output).value, "\"make a toast\"");
    }

    #[tokio::test]
    async fn test_upload_download_keys() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_dir_str = format!("{}", tmp_dir.to_string_lossy());

        let msg_handler = MsgHandler::new(Duration::from_secs(10)).unwrap();
        let cfg = HashMap::from([
            ("first".to_string(), format!("{tmp_dir_str}/first/key")),
            ("second".to_string(), format!("{tmp_dir_str}/second/key")),
            ("third".to_string(), format!("{tmp_dir_str}/third/key")),
        ]);

        println!("no files uploaded yet");
        let output = msg_handler
            .handle(BabelRequest::DownloadKeys(cfg.clone()))
            .await
            .unwrap();
        let keys = unwrap_keys(output);
        assert_eq!(keys.len(), 0);

        println!("upload bad keys");
        let err = msg_handler
            .handle(BabelRequest::UploadKeys((cfg.clone(), vec![])))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Keys management error: No keys provided");

        println!("upload good keys");
        let output = msg_handler
            .handle(BabelRequest::UploadKeys((
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
            .handle(BabelRequest::DownloadKeys(cfg))
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

        let cfg = HashMap::from([(
            WILDCARD_KEY_NAME.to_string(),
            format!("{tmp_dir_str}/star/"),
        )]);
        let msg_handler = MsgHandler::new(Duration::from_secs(10)).unwrap();

        println!("upload unknown keys");
        let output = msg_handler
            .handle(BabelRequest::UploadKeys((
                cfg.clone(),
                vec![BlockchainKey {
                    name: "unknown".to_string(),
                    content: b"12345".to_vec(),
                }],
            )))
            .await
            .unwrap();
        assert_eq!(
            unwrap_blockchain(output).value,
            format!("Done writing 5 bytes of key `unknown` into `{tmp_dir_str}/star/unknown`")
        );

        println!("files in star dir should not be downloading");
        let output = msg_handler
            .handle(BabelRequest::DownloadKeys(cfg))
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

        let json_cmd = BabelRequest::BlockchainRest {
            url: format!("http://{}/items", server.address()),
            response: RestResponse {
                status: 101,
                field: Some("result".to_string()),
                format: MethodResponseFormat::Json,
            },
        };
        let msg_handler = MsgHandler::new(Duration::from_secs(1)).unwrap();
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

        let json_cmd = BabelRequest::BlockchainRest {
            url: format!("http://{}/items", server.address()),
            response: RestResponse {
                status: 101,
                field: None,
                format: MethodResponseFormat::Json,
            },
        };
        let msg_handler = MsgHandler::new(Duration::from_secs(1)).unwrap();
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

        let height_cmd = BabelRequest::BlockchainJrpc {
            host: format!("http://{}", server.address()),
            method: "info_get".to_string(),
            response: JrpcResponse {
                code: 101,
                field: Some("result.info.height".to_string()),
            },
        };
        let msg_handler = MsgHandler::new(Duration::from_secs(1)).unwrap();
        let output = msg_handler.handle(height_cmd).await.unwrap();

        mock.assert();
        assert_eq!(unwrap_blockchain(output).value, "123");
    }
}
