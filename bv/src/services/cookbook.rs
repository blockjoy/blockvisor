use crate::{
    node::{KERNEL_FILE, ROOT_FS_FILE},
    node_data::NodeImage,
    services::api::with_auth,
    utils, with_retry, BV_VAR_PATH,
};
use anyhow::{Context, Result};
use babel_api::config::Babel;
use std::path::{Path, PathBuf};
use tokio::fs::{self, DirBuilder, File};
use tokio::io::AsyncWriteExt;
use tonic::transport::Channel;
use tracing::{debug, info, instrument};

pub mod cb_pb {
    tonic::include_proto!("blockjoy.api.v1.babel");
}

pub const IMAGES_DIR: &str = "images";
const BABEL_ARCHIVE_IMAGE_NAME: &str = "blockjoy.gz";
const BABEL_IMAGE_NAME: &str = "blockjoy";
const KERNEL_ARCHIVE_NAME: &str = "kernel.gz";
const BABEL_CONFIG_NAME: &str = "babel.toml";

pub struct CookbookService {
    token: String,
    client: cb_pb::cook_book_service_client::CookBookServiceClient<Channel>,
    bv_root: PathBuf,
}

impl CookbookService {
    pub async fn connect(bv_root: PathBuf, url: &str, token: &str) -> Result<Self> {
        let client =
            cb_pb::cook_book_service_client::CookBookServiceClient::connect(url.to_string())
                .await
                .context(format!("Failed to connect to cookbook service at {url}"))?;

        Ok(Self {
            token: token.to_string(),
            client,
            bv_root,
        })
    }

    #[instrument(skip(self))]
    pub async fn list_versions(&mut self, protocol: &str, node_type: &str) -> Result<Vec<String>> {
        info!("Listing versions...");
        let req = cb_pb::BabelVersionsRequest {
            protocol: protocol.to_string(),
            node_type: node_type.to_string(),
            status: cb_pb::StatusName::Development.into(),
        };

        let resp = with_retry!(self
            .client
            .list_babel_versions(with_auth(req.clone(), &self.token)))?
        .into_inner();

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
    pub async fn download_babel_config(&mut self, image: &NodeImage) -> Result<()> {
        info!("Downloading config...");
        let babel: cb_pb::Configuration = with_retry!(self
            .client
            .retrieve_configuration(with_auth(image.clone().into(), &self.token)))?
        .into_inner();

        let folder = Self::get_image_download_folder_path(&self.bv_root, image);
        DirBuilder::new().recursive(true).create(&folder).await?;
        let path = folder.join(BABEL_CONFIG_NAME);
        let mut f = File::create(path).await?;
        f.write_all(&babel.toml_content).await?;
        debug!("Done downloading config");

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn download_image(&mut self, image: &NodeImage) -> Result<()> {
        info!("Downloading image...");
        let archive: cb_pb::ArchiveLocation = with_retry!(self
            .client
            .retrieve_image(with_auth(image.clone().into(), &self.token)))?
        .into_inner();

        let folder = Self::get_image_download_folder_path(&self.bv_root, image);
        DirBuilder::new().recursive(true).create(&folder).await?;
        let path = folder.join(ROOT_FS_FILE);
        let gz = folder.join(BABEL_ARCHIVE_IMAGE_NAME);
        utils::download_archive_with_retry(&archive.url, gz)
            .await?
            .ungzip()
            .await?;
        // TODO: change ROOT_FS_FILE to 'blockjoy' to skip that
        tokio::fs::rename(folder.join(BABEL_IMAGE_NAME), path).await?;
        debug!("Done downloading image");

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn download_kernel(&mut self, image: &NodeImage) -> Result<()> {
        info!("Downloading kernel...");
        let archive: cb_pb::ArchiveLocation = with_retry!(self
            .client
            .retrieve_kernel(with_auth(image.clone().into(), &self.token)))?
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

    #[instrument]
    pub async fn get_babel_config(bv_root: &Path, image: &NodeImage) -> Result<Babel> {
        info!("Reading babel config...");

        let folder = Self::get_image_download_folder_path(bv_root, image);
        let path = folder.join(BABEL_CONFIG_NAME);
        let config = fs::read_to_string(path).await?;

        Ok(toml::from_str(&config)?)
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
        let config = folder.join(BABEL_CONFIG_NAME);
        if !root.exists() || !kernel.exists() || !config.exists() {
            return Ok(false);
        }

        Ok(fs::metadata(root).await?.len() > 0
            && fs::metadata(kernel).await?.len() > 0
            && fs::metadata(config).await?.len() > 0)
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
