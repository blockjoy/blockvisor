use crate::{
    api_with_retry,
    node_data::NodeImage,
    services::{
        api::{common, pb, pb::blockchain_service_client},
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
    utils, BV_VAR_PATH,
};
use eyre::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};
use tonic::transport::Channel;
use tracing::{debug, info, instrument};

pub const IMAGES_DIR: &str = "images";
pub const BABEL_ARCHIVE_IMAGE_NAME: &str = "blockjoy.gz";
pub const BABEL_IMAGE_NAME: &str = "blockjoy";
pub const BABEL_PLUGIN_NAME: &str = "babel.rhai";
pub const ROOT_FS_FILE: &str = "os.img";
pub const DATA_FILE: &str = "data.img";

pub struct BlockchainService<C> {
    connector: C,
    bv_root: PathBuf,
}

type BlockchainServiceClient =
    blockchain_service_client::BlockchainServiceClient<AuthenticatedService>;

impl<C: ApiServiceConnector + Clone> BlockchainService<C> {
    pub async fn new(connector: C, bv_root: PathBuf) -> Result<Self> {
        Ok(Self { connector, bv_root })
    }

    async fn connect_blockchain_service(
        &self,
    ) -> Result<
        ApiClient<
            BlockchainServiceClient,
            impl ApiServiceConnector,
            impl Fn(Channel, ApiInterceptor) -> BlockchainServiceClient + Clone,
        >,
    > {
        ApiClient::build(
            self.connector.clone(),
            blockchain_service_client::BlockchainServiceClient::with_interceptor,
        )
        .await
        .with_context(|| "cannot connect to blockchain service")
    }

    #[instrument(skip(self))]
    pub async fn list_image_versions(
        &mut self,
        protocol: &str,
        node_type: &str,
    ) -> Result<Vec<String>> {
        info!("Listing versions...");
        let mut client = self.connect_blockchain_service().await?;
        let mut req = pb::BlockchainServiceListImageVersionsRequest {
            protocol: protocol.to_string(),
            node_type: 0,
        };
        req.set_node_type(node_type.parse()?);

        let resp = api_with_retry!(client, client.list_image_versions(req.clone()))?.into_inner();

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
        let mut client = self.connect_blockchain_service().await?;
        let rhai_content = api_with_retry!(
            client,
            client.get_plugin(tonic::Request::new(pb::BlockchainServiceGetPluginRequest {
                id: Some(image.clone().try_into()?),
            }))
        )?
        .into_inner()
        .plugin
        .ok_or_else(|| anyhow!("missing plugin"))?
        .rhai_content;

        let folder = get_image_download_folder_path(&self.bv_root, image);
        fs::create_dir_all(&folder).await?;
        let path = folder.join(BABEL_PLUGIN_NAME);
        let mut f = File::create(path).await?;
        f.write_all(&rhai_content).await?;
        debug!("Done downloading plugin");

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn download_image(&mut self, image: &NodeImage) -> Result<()> {
        info!("Downloading image...");
        let mut client = self.connect_blockchain_service().await?;
        let archive: common::ArchiveLocation = api_with_retry!(
            client,
            client.get_image(tonic::Request::new(pb::BlockchainServiceGetImageRequest {
                id: Some(image.clone().try_into()?),
            }))
        )?
        .into_inner()
        .location
        .ok_or_else(|| anyhow!("missing location"))?;

        let folder = get_image_download_folder_path(&self.bv_root, image);
        fs::create_dir_all(&folder).await?;
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
    let folder = get_image_download_folder_path(bv_root, image);

    let root = folder.join(ROOT_FS_FILE);
    let plugin = folder.join(BABEL_PLUGIN_NAME);
    if !root.exists() || !plugin.exists() {
        return Ok(false);
    }

    Ok(fs::metadata(root).await?.len() > 0 && fs::metadata(plugin).await?.len() > 0)
}
