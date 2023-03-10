use crate::ufw_wrapper::apply_firewall_config;
use async_trait::async_trait;
use babel_api::BlockchainKey;
use eyre::{bail, Result};
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio::{
    fs,
    fs::{DirBuilder, File},
    io::AsyncWriteExt,
};
use tonic::{Code, Request, Response, Status};

const WILDCARD_KEY_NAME: &str = "*";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct BabelService {
    inner: reqwest::Client,
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
                Status::new(
                    Code::Internal,
                    format!("failed to apply firewall config with: {err}"),
                )
            })?;
        Ok(Response::new(()))
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
    ) -> Result<Response<babel_api::BlockchainResponse>, Status> {
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

        Ok(Response::new(babel_api::BlockchainResponse::Ok(
            results.join("\n"),
        )))
    }

    async fn blockchain_jrpc(
        &self,
        request: Request<(String, String, babel_api::config::JrpcResponse)>,
    ) -> Result<Response<babel_api::BlockchainResponse>, Status> {
        Ok(Response::new(to_blockchain_response(
            self.handle_jrpc(request).await,
        )))
    }

    async fn blockchain_rest(
        &self,
        request: Request<(String, babel_api::config::RestResponse)>,
    ) -> Result<Response<babel_api::BlockchainResponse>, Status> {
        Ok(Response::new(to_blockchain_response(
            self.handle_rest(request).await,
        )))
    }

    async fn blockchain_sh(
        &self,
        request: Request<(String, babel_api::config::ShResponse)>,
    ) -> Result<Response<babel_api::BlockchainResponse>, Status> {
        Ok(Response::new(to_blockchain_response(
            self.handle_sh(request).await,
        )))
    }
}

fn to_blockchain_response(output: Result<String>) -> babel_api::BlockchainResponse {
    output.map_err(|err| err.to_string())
}

impl BabelService {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { inner: client })
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
    use babel_api::babel_server::Babel;
    use babel_api::config::{JrpcResponse, MethodResponseFormat, RestResponse, ShResponse};
    use httpmock::prelude::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_upload_download_keys() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_dir_str = format!("{}", tmp_dir.to_string_lossy());

        let service = BabelService::new()?;
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
            .into_inner()
            .unwrap();
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
        let service = BabelService::new()?;

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
            .into_inner()
            .unwrap();

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

        let service = BabelService::new()?;
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
            .into_inner()
            .unwrap();

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

        let service = BabelService::new()?;
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
            .into_inner()
            .unwrap();

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

        let service = BabelService::new()?;
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
            .into_inner()
            .unwrap();

        mock.assert();
        assert_eq!(output, "{\"result\":[1,2,3]}");
        Ok(())
    }

    #[tokio::test]
    async fn test_sh() -> Result<()> {
        let service = BabelService::new()?;

        let output = service
            .blockchain_sh(Request::new((
                "echo \\\"make a toast\\\"".to_string(),
                ShResponse {
                    status: 102,
                    format: MethodResponseFormat::Json,
                },
            )))
            .await?
            .into_inner()
            .unwrap();
        assert_eq!(output, "\"make a toast\"");
        Ok(())
    }
}
