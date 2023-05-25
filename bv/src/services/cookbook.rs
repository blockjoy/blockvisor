use crate::{
    config::SharedConfig,
    node::{KERNEL_FILE, ROOT_FS_FILE},
    node_data::NodeImage,
    services,
    services::api::{self, AuthenticatedService},
    utils, with_retry, BV_VAR_PATH,
};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::path::{Path, PathBuf};
use tokio::{
    fs::{self, DirBuilder, File},
    io::AsyncWriteExt,
};
use tonic::transport::Endpoint;
use tracing::{debug, info, instrument};

pub mod cb_pb {
    tonic::include_proto!("blockjoy.api.v1.babel");
}

pub const IMAGES_DIR: &str = "images";
pub const COOKBOOK_TOKEN: &str =
    "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzUxMiJ9.eyJpZCI6ImMzNjFjYTY2LWFmZDMtNGNhNy1iMDgwLWFkYTBhZTMyNmRlO\
    SIsImV4cCI6MTcxNjA1MDIxNSwidG9rZW5fdHlwZSI6Imhvc3RfYXV0aCIsInJvbGUiOiJzZXJ2aWNlIn0.avtSu_nmQrtw\
    I-LSFXba3yGHn2PlBVIjBpY4v3kRr9V_eHDdbRqj7-LHH7HH7vOJKSMMFrOhxm6XSIAdbhoSHw";
const BABEL_ARCHIVE_IMAGE_NAME: &str = "blockjoy.gz";
const BABEL_IMAGE_NAME: &str = "blockjoy";
const KERNEL_ARCHIVE_NAME: &str = "kernel.gz";
pub const BABEL_PLUGIN_NAME: &str = "babel.rhai";

pub struct CookbookService {
    client: cb_pb::cook_book_service_client::CookBookServiceClient<AuthenticatedService>,
    bv_root: PathBuf,
}

impl CookbookService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        services::connect(config, |config| async {
            let url = config
                .read()
                .await
                .blockjoy_registry_url
                .ok_or_else(|| anyhow!("missing blockjoy_registry_url"))?;
            let endpoint = Endpoint::from_shared(url.clone())?;
            let channel = Endpoint::connect(&endpoint)
                .await
                .with_context(|| format!("Failed to connect to cookbook service at {url}"))?;
            let client = cb_pb::cook_book_service_client::CookBookServiceClient::with_interceptor(
                channel,
                api::AuthToken(STANDARD.encode(config.read().await.cookbook_token)),
            );
            Ok(Self {
                client,
                bv_root: config.bv_root.clone(),
            })
        })
        .await
    }

    #[instrument(skip(self))]
    pub async fn list_versions(&mut self, protocol: &str, node_type: &str) -> Result<Vec<String>> {
        info!("Listing versions...");
        let req = cb_pb::BabelVersionsRequest {
            protocol: protocol.to_string(),
            node_type: node_type.to_string(),
            status: cb_pb::StatusName::Development.into(),
        };

        let resp = with_retry!(self.client.list_babel_versions(req.clone()))?.into_inner();

        let mut versions: Vec<String> = resp
            .identifiers
            .into_iter()
            .map(|id| id.node_version)
            .collect();
        // sort desc
        versions.sort_by(|a, b| utils::semver_cmp(b, a));

        Ok(versions)
    }

    #[instrument(skip(self))]
    pub async fn download_babel_plugin(&mut self, image: &NodeImage) -> Result<()> {
        info!("Downloading plugin...");
        let rhai_content = with_retry!(self
            .client
            .retrieve_plugin(tonic::Request::new(image.clone().into())))?
        .into_inner()
        .rhai_content;

        let folder = Self::get_image_download_folder_path(&self.bv_root, image);
        DirBuilder::new().recursive(true).create(&folder).await?;
        let path = folder.join(BABEL_PLUGIN_NAME);
        let mut f = File::create(path).await?;
        f.write_all(&rhai_content).await?;
        debug!("Done downloading plugin");

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn download_image(&mut self, image: &NodeImage) -> Result<()> {
        info!("Downloading image...");
        let archive: cb_pb::ArchiveLocation = with_retry!(self
            .client
            .retrieve_image(tonic::Request::new(image.clone().into())))?
        .into_inner();

        let folder = Self::get_image_download_folder_path(&self.bv_root, image);
        DirBuilder::new().recursive(true).create(&folder).await?;
        let path = folder.join(ROOT_FS_FILE);
        let gz = folder.join(BABEL_ARCHIVE_IMAGE_NAME);
        utils::download_archive_with_retry(&archive.url, gz)
            .await?
            .ungzip()
            .await?;
        tokio::fs::rename(folder.join(BABEL_IMAGE_NAME), path).await?;
        debug!("Done downloading image");

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn download_kernel(&mut self, image: &NodeImage) -> Result<()> {
        info!("Downloading kernel...");
        let archive: cb_pb::ArchiveLocation = with_retry!(self
            .client
            .retrieve_kernel(tonic::Request::new(image.clone().into())))?
        .into_inner();

        let folder = Self::get_image_download_folder_path(&self.bv_root, image);
        DirBuilder::new().recursive(true).create(&folder).await?;
        let gz = folder.join(KERNEL_ARCHIVE_NAME);
        utils::download_archive_with_retry(&archive.url, gz)
            .await?
            .ungzip()
            .await?;
        debug!("Done downloading kernel");

        Ok(())
    }

    pub fn get_image_download_folder_path(bv_root: &Path, image: &NodeImage) -> PathBuf {
        bv_root
            .join(BV_VAR_PATH)
            .join(IMAGES_DIR)
            .join(&image.protocol)
            .join(&image.node_type)
            .join(&image.node_version)
    }

    pub async fn is_image_cache_valid(bv_root: &Path, image: &NodeImage) -> Result<bool> {
        let folder = CookbookService::get_image_download_folder_path(bv_root, image);

        let root = folder.join(ROOT_FS_FILE);
        let kernel = folder.join(KERNEL_FILE);
        let plugin = folder.join(BABEL_PLUGIN_NAME);
        if !root.exists() || !kernel.exists() || !plugin.exists() {
            return Ok(false);
        }

        Ok(fs::metadata(root).await?.len() > 0
            && fs::metadata(kernel).await?.len() > 0
            && fs::metadata(plugin).await?.len() > 0)
    }
}

impl From<NodeImage> for cb_pb::ConfigIdentifier {
    fn from(image: NodeImage) -> Self {
        Self {
            protocol: image.protocol,
            node_type: image.node_type,
            node_version: image.node_version,
            status: cb_pb::StatusName::Development.into(),
        }
    }
}
