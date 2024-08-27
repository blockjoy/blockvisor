use crate::{
    api_with_retry,
    config::SharedConfig,
    services::{
        self,
        api::{pb, pb::archive_service_client},
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
};
use babel_api::engine::{
    Checksum, Chunk, Compression, DownloadManifest, DownloadMetadata, FileLocation, Slot,
    UploadSlots,
};
use bv_utils::rpc::with_timeout;
use eyre::{anyhow, bail, Context, Result};
use std::{path::PathBuf, str::FromStr, time::Duration};
use tonic::transport::Channel;
use tracing::info;
use url::Url;

type ArchiveServiceClient = archive_service_client::ArchiveServiceClient<AuthenticatedService>;

async fn connect_blockchain_archive_service(
    config: &SharedConfig,
) -> Result<
    ApiClient<
        ArchiveServiceClient,
        impl ApiServiceConnector,
        impl Fn(Channel, ApiInterceptor) -> ArchiveServiceClient + Clone,
    >,
> {
    ApiClient::build_with_default_connector(config, |channel, api_interceptor| {
        archive_service_client::ArchiveServiceClient::with_interceptor(channel, api_interceptor)
            .send_compressed(tonic::codec::CompressionEncoding::Gzip)
            .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
    })
    .await
    .with_context(|| "cannot connect to blockchain archive service")
}

pub async fn put_download_manifest(
    config: &SharedConfig,
    archive_id: String,
    manifest: DownloadManifest,
    data_version: u64,
) -> Result<()> {
    info!("Putting download manifest...");
    let mut client = connect_blockchain_archive_service(config).await?;
    // DownloadManifest may be pretty big, so better set longer timeout that depends on number of chunks
    let custom_timeout =
        bv_utils::rpc::estimate_put_download_manifest_request_timeout(manifest.chunks.len());
    let compression = manifest.compression.map(|v| v.into());
    let chunks = manifest
        .chunks
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let checksum = value.checksum.into();
            let destinations = value
                .destinations
                .into_iter()
                .map(|value| pb::ChunkTarget {
                    path: value.path.to_string_lossy().to_string(),
                    position_bytes: value.pos,
                    size_bytes: value.size,
                })
                .collect();
            pb::ArchiveChunk {
                index: index as u32,
                key: value.key,
                url: value.url.map(|url| url.to_string()),
                checksum: Some(checksum),
                size: value.size,
                destinations,
            }
        })
        .collect();
    let manifest = pb::ArchiveServicePutDownloadManifestRequest {
        archive_id,
        data_version,
        org_id: None,
        total_size: manifest.total_size,
        compression,
        chunks,
    };
    api_with_retry!(
        client,
        client.put_download_manifest(with_timeout(manifest.clone(), custom_timeout))
    )
    .with_context(|| "put_download_manifest failed")?;
    Ok(())
}

pub async fn get_download_metadata(
    config: &SharedConfig,
    archive_id: String,
) -> Result<DownloadMetadata> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_download_metadata(with_timeout(
            pb::ArchiveServiceGetDownloadMetadataRequest {
                archive_id: archive_id.clone(),
                org_id: None,
                data_version: None,
            },
            // checking download manifest require validity check, which may be time-consuming
            // let's give it a minute
            Duration::from_secs(60),
        ))
    )
    .with_context(|| "cannot get download metadata")?
    .into_inner()
    .try_into()
}

pub async fn get_download_chunks(
    config: &SharedConfig,
    archive_id: String,
    data_version: u64,
    chunk_indexes: Vec<u32>,
) -> Result<Vec<Chunk>> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_download_chunks(with_timeout(
            pb::ArchiveServiceGetDownloadChunksRequest {
                archive_id: archive_id.clone(),
                org_id: None,
                data_version,
                chunk_indexes: chunk_indexes.clone(),
            },
            // checking download manifest require validity check, which may be time-consuming
            // let's give it a minute
            Duration::from_secs(60),
        ))
    )
    .with_context(|| "cannot get download chunks")?
    .into_inner()
    .chunks
    .into_iter()
    .map(|value| value.try_into())
    .collect::<Result<Vec<_>>>()
}

pub async fn has_blockchain_archive(config: &SharedConfig, archive_id: String) -> Result<bool> {
    let mut client = connect_blockchain_archive_service(config).await?;
    if let Err(err) = api_with_retry!(
        client,
        client.get_download_metadata(with_timeout(
            pb::ArchiveServiceGetDownloadMetadataRequest {
                archive_id: archive_id.clone(),
                org_id: None,
                data_version: None,
            },
            // checking download manifest require validity check, which may be time-consuming
            // let's give it a minute
            Duration::from_secs(60),
        ))
    ) {
        if err.code() == tonic::Code::NotFound {
            Ok(false)
        } else {
            bail!("cannot check blockchain archive: {err:#}");
        }
    } else {
        Ok(true)
    }
}

pub async fn get_upload_slots(
    config: &SharedConfig,
    archive_id: String,
    data_version: Option<u64>,
    slots: Vec<u32>,
    url_expires: u32,
) -> Result<UploadSlots> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_upload_slots(with_timeout(
            pb::ArchiveServiceGetUploadSlotsRequest {
                archive_id: archive_id.clone(),
                org_id: None,
                data_version,
                slot_indexes: slots.clone(),
                url_expires: Some(url_expires),
            },
            // let make timeout proportional to number of slots
            // it is expected that 1000 of slots should be downloaded in less thant 5s
            services::DEFAULT_API_REQUEST_TIMEOUT + Duration::from_secs(slots.len() as u64 / 200),
        ))
    )
    .with_context(|| "cannot retrieve upload slots")?
    .into_inner()
    .try_into()
}

impl From<pb::compression::Compression> for Compression {
    fn from(value: pb::compression::Compression) -> Self {
        match value {
            pb::compression::Compression::Zstd(level) => Compression::ZSTD(level),
        }
    }
}

impl TryFrom<pb::checksum::Checksum> for Checksum {
    type Error = eyre::Error;
    fn try_from(value: pb::checksum::Checksum) -> Result<Self, Self::Error> {
        Ok(match value {
            pb::checksum::Checksum::Sha1(bytes) => Checksum::Sha1(try_into_array(bytes)?),
            pb::checksum::Checksum::Sha256(bytes) => Checksum::Sha256(try_into_array(bytes)?),
            pb::checksum::Checksum::Blake3(bytes) => Checksum::Blake3(try_into_array(bytes)?),
        })
    }
}

impl TryFrom<pb::ArchiveServiceGetDownloadMetadataResponse> for DownloadMetadata {
    type Error = eyre::Error;
    fn try_from(
        metadata: pb::ArchiveServiceGetDownloadMetadataResponse,
    ) -> Result<Self, Self::Error> {
        let compression = if let Some(pb::Compression {
            compression: Some(compression),
        }) = metadata.compression
        {
            Some(compression.into())
        } else {
            None
        };
        Ok(Self {
            total_size: metadata.total_size,
            compression,
            chunks: metadata.chunks,
            data_version: metadata.data_version,
        })
    }
}

impl TryFrom<pb::ArchiveChunk> for Chunk {
    type Error = eyre::Error;
    fn try_from(chunk: pb::ArchiveChunk) -> Result<Self, Self::Error> {
        let checksum = if let Some(pb::Checksum {
            checksum: Some(checksum),
        }) = chunk.checksum
        {
            checksum.try_into()?
        } else {
            bail!("Missing checksum")
        };
        let destinations = chunk
            .destinations
            .into_iter()
            .map(|value| {
                Ok(FileLocation {
                    path: PathBuf::from_str(&value.path)?,
                    pos: value.position_bytes,
                    size: value.size_bytes,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            index: chunk.index,
            key: chunk.key,
            url: chunk.url.map(|url| Url::parse(&url)).transpose()?,
            checksum,
            size: chunk.size,
            destinations,
        })
    }
}

impl From<Compression> for pb::Compression {
    fn from(value: Compression) -> Self {
        match value {
            Compression::ZSTD(level) => pb::Compression {
                compression: Some(pb::compression::Compression::Zstd(level)),
            },
        }
    }
}

impl From<Checksum> for pb::Checksum {
    fn from(value: Checksum) -> Self {
        match value {
            Checksum::Sha1(bytes) => pb::Checksum {
                checksum: Some(pb::checksum::Checksum::Sha1(bytes.to_vec())),
            },
            Checksum::Sha256(bytes) => pb::Checksum {
                checksum: Some(pb::checksum::Checksum::Sha256(bytes.to_vec())),
            },
            Checksum::Blake3(bytes) => pb::Checksum {
                checksum: Some(pb::checksum::Checksum::Blake3(bytes.to_vec())),
            },
        }
    }
}

impl TryFrom<pb::ArchiveServiceGetUploadSlotsResponse> for UploadSlots {
    type Error = eyre::Report;
    fn try_from(slots: pb::ArchiveServiceGetUploadSlotsResponse) -> Result<Self> {
        Ok(Self {
            slots: slots
                .slots
                .into_iter()
                .map(|slot| slot.try_into())
                .collect::<Result<Vec<_>>>()?,
            data_version: slots.data_version,
        })
    }
}

impl TryFrom<pb::UploadSlot> for Slot {
    type Error = eyre::Report;
    fn try_from(value: pb::UploadSlot) -> Result<Self> {
        Ok(Self {
            index: value.index,
            key: value.key,
            url: Url::parse(&value.url)?,
        })
    }
}

fn try_into_array<const S: usize>(vec: Vec<u8>) -> Result<[u8; S]> {
    vec.try_into().map_err(|vec: Vec<u8>| {
        anyhow!(
            "expected {} bytes checksum, but {} bytes found",
            S,
            vec.len()
        )
    })
}
