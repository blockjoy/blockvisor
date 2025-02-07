use crate::{
    api_with_retry,
    bv_config::SharedConfig,
    services::{
        api::{common, pb, pb::crypt_service_client},
        ApiClient, ApiInterceptor, ApiServiceConnector, AuthenticatedService,
    },
};
use eyre::{bail, Context, Result};
use tonic::transport::Channel;
use tracing::info;
use uuid::Uuid;

type CryptServiceClient = crypt_service_client::CryptServiceClient<AuthenticatedService>;

async fn connect_crypt_service(
    config: &SharedConfig,
) -> Result<
    ApiClient<
        CryptServiceClient,
        impl ApiServiceConnector,
        impl Fn(Channel, ApiInterceptor) -> CryptServiceClient + Clone,
    >,
> {
    ApiClient::build_with_default_connector(config, |channel, api_interceptor| {
        crypt_service_client::CryptServiceClient::with_interceptor(channel, api_interceptor)
            .send_compressed(tonic::codec::CompressionEncoding::Gzip)
            .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
    })
    .await
    .with_context(|| "cannot connect to crypt service")
}

pub async fn put_secret(
    config: &SharedConfig,
    node_id: Uuid,
    key: &str,
    value: &[u8],
) -> Result<()> {
    info!("Putting node {node_id} secret {key}");
    let mut client = connect_crypt_service(config).await?;
    api_with_retry!(
        client,
        client.put_secret(pb::CryptServicePutSecretRequest {
            resource: Some(common::Resource {
                resource_type: common::ResourceType::Node.into(),
                resource_id: node_id.to_string()
            }),
            key: key.to_string(),
            value: value.to_vec(),
        })
    )
    .with_context(|| "put_key failed")?;
    Ok(())
}

pub async fn get_secret(
    config: &SharedConfig,
    node_id: Uuid,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    info!("Getting node {node_id} secret {name}");
    let mut client = connect_crypt_service(config).await?;
    Ok(
        match api_with_retry!(
            client,
            client.get_secret(pb::CryptServiceGetSecretRequest {
                resource: Some(common::Resource {
                    resource_type: common::ResourceType::Node.into(),
                    resource_id: node_id.to_string()
                }),
                key: name.to_string(),
            })
        ) {
            Ok(value) => Some(value.into_inner().value),
            Err(err) => {
                if err.code() == tonic::Code::NotFound {
                    None
                } else {
                    bail!("get_secret failed: {err:#}");
                }
            }
        },
    )
}
