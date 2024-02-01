use crate::{
    api_with_retry,
    config::SharedConfig,
    node_data::NodeImage,
    services,
    services::{
        api::{pb, pb::blockchain_archive_service_client},
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
};
use babel_api::engine::{DownloadManifest, UploadManifest};
use bv_utils::rpc::with_timeout;
use eyre::{anyhow, Context, Result};
use std::time::Duration;
use tonic::transport::Channel;
use tracing::info;

type BlockchainArchiveServiceClient =
    blockchain_archive_service_client::BlockchainArchiveServiceClient<AuthenticatedService>;

async fn connect_blockchain_archive_service(
    config: &SharedConfig,
) -> Result<
    ApiClient<
        BlockchainArchiveServiceClient,
        impl ApiServiceConnector,
        impl Fn(Channel, ApiInterceptor) -> BlockchainArchiveServiceClient + Clone,
    >,
> {
    ApiClient::build_with_default_connector(config, |channel, api_interceptor| {
        blockchain_archive_service_client::BlockchainArchiveServiceClient::with_interceptor(
            channel,
            api_interceptor,
        )
        .send_compressed(tonic::codec::CompressionEncoding::Gzip)
        .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
    })
    .await
    .with_context(|| "cannot connect to blockchain archive service")
}

pub async fn put_download_manifest(
    config: &SharedConfig,
    image: NodeImage,
    network: String,
    manifest: DownloadManifest,
) -> Result<()> {
    info!("Putting download manifest...");
    let mut client = connect_blockchain_archive_service(config).await?;
    // DownloadManifest may be pretty big, so better set longer timeout that depends on number of chunks
    let custom_timeout =
        bv_utils::rpc::estimate_put_download_manifest_request_timeout(manifest.chunks.len());
    let manifest = pb::BlockchainArchiveServicePutDownloadManifestRequest {
        id: Some(image.try_into()?),
        network,
        manifest: Some(manifest.into()),
    };
    api_with_retry!(
        client,
        client.put_download_manifest(with_timeout(manifest.clone(), custom_timeout))
    )
    .with_context(|| "put_download_manifest failed")?;
    Ok(())
}

pub async fn retrieve_download_manifest(
    config: &SharedConfig,
    image: NodeImage,
    network: String,
) -> Result<DownloadManifest> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_download_manifest(with_timeout(
            pb::BlockchainArchiveServiceGetDownloadManifestRequest {
                id: Some(image.clone().try_into()?),
                network: network.clone(),
            },
            // we don't know how download manifest is big, but it can be pretty big
            // lets give it time that should be enough for 20 000 of chunks ~ 20TB of blockchian data
            Duration::from_secs(200),
        ))
    )
    .with_context(|| {
        format!(
            "cannot retrieve download manifest for {:?}-{}",
            image, network
        )
    })?
    .into_inner()
    .manifest
    .ok_or_else(|| anyhow!("manifest not found for {:?}-{}", image, network))?
    .try_into()
}

pub async fn retrieve_upload_manifest(
    config: &SharedConfig,
    image: NodeImage,
    network: String,
    slots: u32,
    url_expires: u32,
    data_version: Option<u64>,
) -> Result<UploadManifest> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_upload_manifest(with_timeout(
            pb::BlockchainArchiveServiceGetUploadManifestRequest {
                id: Some(image.clone().try_into()?),
                network: network.clone(),
                data_version,
                slots,
                url_expires: Some(url_expires),
            },
            // let make timeout proportional to number of slots
            // it is expected that 1000 of slots should be downloaded in lest thant 5s
            services::DEFAULT_REQUEST_TIMEOUT + Duration::from_secs(slots as u64 / 200),
        ))
    )
    .with_context(|| {
        format!(
            "cannot retrieve upload manifest for {:?}-{}",
            image, network
        )
    })?
    .into_inner()
    .manifest
    .ok_or_else(|| anyhow!("manifest not found for {:?}-{}", image, network))?
    .try_into()
}
