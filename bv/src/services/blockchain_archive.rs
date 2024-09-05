use crate::{
    api_with_retry,
    config::SharedConfig,
    node_state::NodeImage,
    services::{
        self,
        api::{common, pb, pb::blockchain_archive_service_client},
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
};
use babel_api::{
    engine::{
        Checksum, Chunk, Compression, DownloadManifest, DownloadMetadata, FileLocation, Slot,
        UploadSlots,
    },
    metadata::firewall,
};
use bv_utils::rpc::with_timeout;
use eyre::{anyhow, bail, Context, Result};
use reqwest::Url;
use std::{path::PathBuf, str::FromStr, time::Duration};
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
    let manifest = pb::BlockchainArchiveServicePutDownloadManifestRequest {
        id: Some(image.try_into()?),
        network,
        data_version,
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
    image: NodeImage,
    network: String,
) -> Result<DownloadMetadata> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_download_metadata(with_timeout(
            pb::BlockchainArchiveServiceGetDownloadMetadataRequest {
                id: Some(image.clone().try_into()?),
                network: network.clone(),
                data_version: None,
            },
            // checking download manifest require validity check, which may be time-consuming
            // let's give it a minute
            Duration::from_secs(60),
        ))
    )
    .with_context(|| format!("cannot get download metadata for {:?}-{}", image, network))?
    .into_inner()
    .try_into()
}

pub async fn get_download_chunks(
    config: &SharedConfig,
    image: NodeImage,
    network: String,
    data_version: u64,
    chunk_indexes: Vec<u32>,
) -> Result<Vec<Chunk>> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_download_chunks(with_timeout(
            pb::BlockchainArchiveServiceGetDownloadChunksRequest {
                id: Some(image.clone().try_into()?),
                network: network.clone(),
                data_version,
                chunk_indexes: chunk_indexes.clone(),
            },
            // checking download manifest require validity check, which may be time-consuming
            // let's give it a minute
            Duration::from_secs(60),
        ))
    )
    .with_context(|| {
        format!(
            "cannot get download chunks for {:?}-{}/{}",
            image, network, data_version
        )
    })?
    .into_inner()
    .chunks
    .into_iter()
    .map(|value| value.try_into())
    .collect::<Result<Vec<_>>>()
}

pub async fn has_download_manifest(
    config: &SharedConfig,
    image: NodeImage,
    network: String,
) -> Result<bool> {
    let mut client = connect_blockchain_archive_service(config).await?;
    if let Err(err) = api_with_retry!(
        client,
        client.get_download_metadata(with_timeout(
            pb::BlockchainArchiveServiceGetDownloadMetadataRequest {
                id: Some(image.clone().try_into()?),
                network: network.clone(),
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
            bail!(
                "cannot check download metadata for {:?}-{}: {err:#}",
                image,
                network
            );
        }
    } else {
        Ok(true)
    }
}

pub async fn get_upload_slots(
    config: &SharedConfig,
    image: NodeImage,
    network: String,
    data_version: Option<u64>,
    slots: Vec<u32>,
    url_expires: u32,
) -> Result<UploadSlots> {
    let mut client = connect_blockchain_archive_service(config).await?;
    api_with_retry!(
        client,
        client.get_upload_slots(with_timeout(
            pb::BlockchainArchiveServiceGetUploadSlotsRequest {
                id: Some(image.clone().try_into()?),
                network: network.clone(),
                data_version,
                slot_indexes: slots.clone(),
                url_expires: Some(url_expires),
            },
            // let make timeout proportional to number of slots
            // it is expected that 1000 of slots should be downloaded in less thant 5s
            services::DEFAULT_API_REQUEST_TIMEOUT + Duration::from_secs(slots.len() as u64 / 200),
        ))
    )
    .with_context(|| format!("cannot retrieve upload slots for {:?}-{}", image, network))?
    .into_inner()
    .try_into()
}

impl TryFrom<common::FirewallAction> for firewall::Action {
    type Error = eyre::Error;
    fn try_from(value: common::FirewallAction) -> Result<Self, Self::Error> {
        Ok(match value {
            common::FirewallAction::Unspecified => {
                bail!("Invalid Action")
            }
            common::FirewallAction::Allow => firewall::Action::Allow,
            common::FirewallAction::Deny => firewall::Action::Deny,
            common::FirewallAction::Reject => firewall::Action::Reject,
        })
    }
}

impl TryFrom<common::FirewallDirection> for firewall::Direction {
    type Error = eyre::Error;
    fn try_from(value: common::FirewallDirection) -> Result<Self, Self::Error> {
        Ok(match value {
            common::FirewallDirection::Unspecified => {
                bail!("Invalid Direction")
            }
            common::FirewallDirection::Inbound => firewall::Direction::In,
            common::FirewallDirection::Outbound => firewall::Direction::Out,
        })
    }
}

impl TryFrom<common::FirewallProtocol> for firewall::Protocol {
    type Error = eyre::Error;
    fn try_from(value: common::FirewallProtocol) -> Result<Self, Self::Error> {
        Ok(match value {
            common::FirewallProtocol::Unspecified => bail!("Invalid Protocol"),
            common::FirewallProtocol::Tcp => firewall::Protocol::Tcp,
            common::FirewallProtocol::Udp => firewall::Protocol::Udp,
            common::FirewallProtocol::Both => firewall::Protocol::Both,
        })
    }
}

impl TryFrom<common::FirewallRule> for firewall::Rule {
    type Error = eyre::Error;
    fn try_from(rule: common::FirewallRule) -> Result<Self, Self::Error> {
        let action = rule.action().try_into()?;
        let direction = rule.direction().try_into()?;
        let protocol = Some(rule.protocol().try_into()?);
        Ok(Self {
            name: rule.name,
            action,
            direction,
            protocol,
            ips: rule.ips,
            ports: rule.ports.into_iter().map(|p| p as u16).collect(),
        })
    }
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

impl TryFrom<pb::BlockchainArchiveServiceGetDownloadMetadataResponse> for DownloadMetadata {
    type Error = eyre::Error;
    fn try_from(
        metadata: pb::BlockchainArchiveServiceGetDownloadMetadataResponse,
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

impl TryFrom<pb::BlockchainArchiveServiceGetUploadSlotsResponse> for UploadSlots {
    type Error = eyre::Report;
    fn try_from(slots: pb::BlockchainArchiveServiceGetUploadSlotsResponse) -> Result<Self> {
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

fn try_into_array<const S: usize>(vec: Vec<u8>) -> Result<[u8; S]> {
    vec.try_into().map_err(|vec: Vec<u8>| {
        anyhow!(
            "expected {} bytes checksum, but {} bytes found",
            S,
            vec.len()
        )
    })
}
