use crate::{
    config::{Config, SharedConfig},
    services::api::DiscoveryService,
    with_retry,
};
use anyhow::{Context, Result};
use std::future::Future;

pub mod api;
pub mod cookbook;
pub mod keyfiles;
pub mod mqtt;

/// Tries to use connector to create service connection,
/// but if it fails then request backend for new service urls, update `SharedConfig` with them,
/// and try again.
pub async fn connect<S, C, F>(config: SharedConfig, mut connector: C) -> Result<S>
where
    C: FnMut(Config) -> F,
    F: Future<Output = Result<S>>,
{
    if let Ok(service) = connector(config.read().await).await {
        Ok(service)
    } else {
        // if can't connect - refresh urls and try again
        let services = {
            let mut client = DiscoveryService::connect(config.read().await).await?;
            with_retry!(client.get_services()).with_context(|| "get service urls failed")?
        };
        {
            let mut w_lock = config.write().await;
            w_lock.blockjoy_registry_url = Some(services.registry_url);
            w_lock.blockjoy_keys_url = Some(services.key_service_url);
            w_lock.blockjoy_mqtt_url = Some(services.notification_url);
        }
        connector(config.read().await).await
    }
}
