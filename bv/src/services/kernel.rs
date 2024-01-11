use crate::{
    api_with_retry,
    services::{
        self,
        api::{common, pb, pb::kernel_service_client},
        AuthenticatedService,
    },
    utils, BV_VAR_PATH,
};
use eyre::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info};

pub const KERNELS_DIR: &str = "kernels";
const KERNEL_ARCHIVE_NAME: &str = "kernel.gz";
pub const KERNEL_FILE: &str = "kernel";

pub type KernelServiceClient = kernel_service_client::KernelServiceClient<AuthenticatedService>;

pub async fn download_kernel(
    bv_root: &Path,
    connector: impl services::ApiServiceConnector,
    version: &str,
) -> Result<()> {
    info!("Downloading kernel...");
    let mut client = services::ApiClient::build(
        connector,
        pb::kernel_service_client::KernelServiceClient::with_interceptor,
    )
    .await
    .with_context(|| "cannot connect to kernel service")?;
    let archive: common::ArchiveLocation = api_with_retry!(
        client,
        client.retrieve(tonic::Request::new(pb::KernelServiceRetrieveRequest {
            id: Some(pb::KernelIdentifier {
                version: version.to_string()
            }),
        }))
    )?
    .into_inner()
    .location
    .ok_or_else(|| anyhow!("missing location"))?;

    let folder = bv_root.join(BV_VAR_PATH).join(KERNELS_DIR).join(version);
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
    let path = get_kernel_path(bv_root, version);
    if !path.exists() {
        return Ok(false);
    }

    Ok(fs::metadata(path).await?.len() > 0)
}
