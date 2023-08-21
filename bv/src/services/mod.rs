use crate::{config::SharedConfig, services::api::DiscoveryService};
use anyhow::{Context, Result};
use bv_utils::with_retry;
use std::future::Future;

pub mod api;
pub mod cookbook;
pub mod kernel;
pub mod keyfiles;
pub mod mqtt;

/// Tries to use `connector` to create service connection. If this fails then asks the backend for
/// new service urls, update `SharedConfig` with them, and tries again.
pub async fn connect<'a, S, C, F>(config: &'a SharedConfig, mut connector: C) -> Result<S>
where
    C: FnMut(&'a SharedConfig) -> F,
    F: Future<Output = Result<S>> + 'a,
{
    if let Ok(service) = connector(config).await {
        Ok(service)
    } else {
        // if we can't connect - refresh urls and try again
        let services = {
            let mut client = DiscoveryService::connect(config).await?;
            with_retry!(client.get_services()).with_context(|| "get service urls failed")?
        };
        {
            let mut w_lock = config.write().await;
            w_lock.blockjoy_mqtt_url = Some(services.notification_url);
        }
        connector(config).await
    }
}
