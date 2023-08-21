use crate::{
    config::SharedConfig, services, services::api::pb, services::api::AuthenticatedService, utils,
    BV_VAR_PATH,
};
use anyhow::{anyhow, Context, Result};
use bv_utils::with_retry;
use std::path::{Path, PathBuf};
use tokio::fs;
use tonic::transport::Endpoint;
use tracing::{debug, info, instrument};

pub const KERNELS_DIR: &str = "kernels";
const KERNEL_ARCHIVE_NAME: &str = "kernel.gz";
pub const KERNEL_FILE: &str = "kernel";

pub struct KernelService {
    client: pb::kernel_service_client::KernelServiceClient<AuthenticatedService>,
    bv_root: PathBuf,
}

impl KernelService {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        services::connect(config, |config| async {
            let url = config.read().await.blockjoy_api_url;
            let endpoint = Endpoint::from_shared(url.clone())?;
            let channel = Endpoint::connect(&endpoint)
                .await
                .with_context(|| format!("Failed to connect to kernel service at {url}"))?;
            let client = pb::kernel_service_client::KernelServiceClient::with_interceptor(
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
    pub async fn list_versions(&mut self) -> Result<Vec<String>> {
        info!("Listing versions...");
        let req = pb::KernelServiceListKernelVersionsRequest {};
        let resp = with_retry!(self.client.list_kernel_versions(req.clone()))?.into_inner();
        let mut versions: Vec<String> = resp.identifiers.into_iter().map(|id| id.version).collect();
        // sort desc
        versions.sort_by(|a, b| utils::semver_cmp(b, a));

        Ok(versions)
    }

    #[instrument(skip(self))]
    pub async fn download_kernel(&mut self, version: &str) -> Result<()> {
        info!("Downloading kernel...");
        let archive: pb::ArchiveLocation = with_retry!(self.client.retrieve(tonic::Request::new(
            pb::KernelServiceRetrieveRequest {
                id: Some(pb::KernelIdentifier {
                    version: version.to_string()
                }),
            }
        )))?
        .into_inner()
        .location
        .ok_or_else(|| anyhow!("missing location"))?;

        let folder = self
            .bv_root
            .join(BV_VAR_PATH)
            .join(KERNELS_DIR)
            .join(version);
        fs::create_dir_all(&folder).await?;
        let gz = folder.join(KERNEL_ARCHIVE_NAME);
        utils::download_archive_with_retry(&archive.url, gz)
            .await?
            .ungzip()
            .await?;
        debug!("Done downloading kernel");

        Ok(())
    }

    pub fn get_kernel_path(bv_root: &Path, version: &str) -> PathBuf {
        bv_root
            .join(BV_VAR_PATH)
            .join(KERNELS_DIR)
            .join(version)
            .join(KERNEL_FILE)
    }

    pub async fn is_kernel_cache_valid(bv_root: &Path, version: &str) -> Result<bool> {
        let path = Self::get_kernel_path(bv_root, version);
        if !path.exists() {
            return Ok(false);
        }

        Ok(fs::metadata(path).await?.len() > 0)
    }
}
