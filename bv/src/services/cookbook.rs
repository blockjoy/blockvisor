use crate::{
    config::SharedConfig, node_data::NodeImage, services, services::api::pb,
    services::api::AuthenticatedService, utils, BV_VAR_PATH,
};
use anyhow::{anyhow, Context, Result};
use bv_utils::with_retry;
use std::path::{Path, PathBuf};
use tokio::{
    fs::{self, DirBuilder, File},
    io::AsyncWriteExt,
};
use tonic::transport::Endpoint;
use tracing::{debug, info, instrument};

pub const IMAGES_DIR: &str = "images";
const BABEL_ARCHIVE_IMAGE_NAME: &str = "blockjoy.gz";
const BABEL_IMAGE_NAME: &str = "blockjoy";
const KERNEL_ARCHIVE_NAME: &str = "kernel.gz";
pub const BABEL_PLUGIN_NAME: &str = "babel.rhai";
pub const ROOT_FS_FILE: &str = "os.img";
pub const KERNEL_FILE: &str = "kernel";
pub const DATA_FILE: &str = "data.img";

pub struct CookbookService {
    client: pb::cookbook_service_client::CookbookServiceClient<AuthenticatedService>,
    bv_root: PathBuf,
}

impl CookbookService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        services::connect(config, |config| async {
            let url = config.read().await.blockjoy_api_url;
            let endpoint = Endpoint::from_shared(url.clone())?;
            let channel = Endpoint::connect(&endpoint)
                .await
                .with_context(|| format!("Failed to connect to cookbook service at {url}"))?;
            let client = pb::cookbook_service_client::CookbookServiceClient::with_interceptor(
                channel,
                config.token().await?,
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
        let req = pb::CookbookServiceListBabelVersionsRequest {
            protocol: protocol.to_string(),
            node_type: node_type.to_string(),
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
        let rhai_content = with_retry!(self.client.retrieve_plugin(tonic::Request::new(
            pb::CookbookServiceRetrievePluginRequest {
                id: Some(image.clone().into()),
            }
        )))?
        .into_inner()
        .plugin
        .ok_or_else(|| anyhow!("missing plugin"))?
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
        let archive: pb::ArchiveLocation = with_retry!(self.client.retrieve_image(
            tonic::Request::new(pb::CookbookServiceRetrieveImageRequest {
                id: Some(image.clone().into()),
            })
        ))?
        .into_inner()
        .location
        .ok_or_else(|| anyhow!("missing location"))?;

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
        let archive: pb::ArchiveLocation = with_retry!(self.client.retrieve_kernel(
            tonic::Request::new(pb::CookbookServiceRetrieveKernelRequest {
                id: Some(image.clone().into()),
            })
        ))?
        .into_inner()
        .location
        .ok_or_else(|| anyhow!("missing location"))?;

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
