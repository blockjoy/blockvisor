use crate::{config::SharedConfig, services::api::pb};
use async_trait::async_trait;
use base64::Engine;
use bv_utils::{rpc::DefaultTimeout, with_retry};
use eyre::{Context, Result};
use std::ops::{Deref, DerefMut};
use std::{future::Future, str::FromStr, time::Duration};
use tonic::{
    codegen::InterceptedService, service::Interceptor, transport::Channel, transport::Endpoint,
    Request, Status,
};

pub mod api;
pub mod blockchain;
pub mod blockchain_archive;
pub mod kernel;
pub mod mqtt;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const TOKEN_EXPIRED_MESSAGE: &str = "TOKEN_EXPIRED";
lazy_static::lazy_static! {
    pub static ref NON_RETRIABLE: Vec<tonic::Code> = vec![tonic::Code::Unauthenticated, tonic::Code::InvalidArgument,
        tonic::Code::Unimplemented, tonic::Code::PermissionDenied];
}

#[macro_export]
macro_rules! api_with_retry {
    ($client:expr, $fun:expr) => {{
        match bv_utils::with_selective_retry!(
            $fun,
            $crate::services::NON_RETRIABLE,
            $client.rpc_retry_max(),
            $client.rpc_backoff_base_ms()
        ) {
            Ok(resp) => Ok(resp),
            Err(status)
                if status.code() == tonic::Code::Unauthenticated
                    && status
                        .message()
                        .contains($crate::services::TOKEN_EXPIRED_MESSAGE) =>
            {
                match $client.refresh().await {
                    Ok(()) => bv_utils::with_selective_retry!(
                        $fun,
                        $crate::services::NON_RETRIABLE,
                        $client.rpc_retry_max(),
                        $client.rpc_backoff_base_ms()
                    ),
                    Err(err) => Err(err),
                }
            }
            Err(status) => Err(status),
        }
    }};
}

pub async fn request_refresh_token(
    url: &str,
    token: &str,
    refresh: &str,
) -> Result<pb::AuthServiceRefreshResponse, Status> {
    let channel = Endpoint::from_str(url)
        .map_err(|err| Status::internal(err.to_string()))?
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .connect_lazy();
    let req = pb::AuthServiceRefreshRequest {
        token: token.to_string(),
        refresh: Some(refresh.to_string()),
    };
    let mut client = pb::auth_service_client::AuthServiceClient::with_interceptor(
        channel,
        DefaultTimeout(DEFAULT_REQUEST_TIMEOUT),
    );

    Ok(with_retry!(client.refresh(req.clone()))?.into_inner())
}

pub struct ApiClient<T, C, I> {
    client: T,
    connector: C,
    with_interceptor: I,
}
impl<T, C, I> ApiClient<T, C, I>
where
    I: Fn(Channel, ApiInterceptor) -> T + Clone + Send + Sync,
    C: ApiServiceConnector,
{
    pub async fn build(connector: C, with_interceptor: I) -> Result<Self, Status> {
        let client = connector.connect(with_interceptor.clone()).await?;
        Ok(Self {
            connector,
            with_interceptor,
            client,
        })
    }

    pub async fn refresh(&mut self) -> Result<(), Status> {
        let client = self
            .connector
            .connect(self.with_interceptor.clone())
            .await?;
        self.client = client;
        Ok(())
    }

    pub fn rpc_retry_max(&self) -> u32 {
        3
    }
    pub fn rpc_backoff_base_ms(&self) -> u64 {
        500
    }
}

impl<T, I> ApiClient<T, DefaultConnector, I>
where
    I: Fn(Channel, crate::services::ApiInterceptor) -> T + Clone + Send + Sync,
{
    pub async fn build_with_default_connector(
        config: &SharedConfig,
        with_interceptor: I,
    ) -> Result<Self, Status> {
        let connector = crate::services::DefaultConnector {
            config: config.clone(),
        };
        let client = connector.connect(with_interceptor.clone()).await?;
        Ok(Self {
            connector,
            with_interceptor,
            client,
        })
    }
}

impl<T, I, C> Deref for ApiClient<T, I, C> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl<T, I, C> DerefMut for ApiClient<T, I, C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

pub async fn connect_to_api_service<T, I>(
    config: &SharedConfig,
    with_interceptor: I,
) -> Result<T, Status>
where
    I: Fn(Channel, ApiInterceptor) -> T,
{
    let url = config.read().await.blockjoy_api_url;
    let endpoint = Endpoint::from_str(&url)
        .map_err(|err| Status::internal(err.to_string()))?
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT);
    let channel = Endpoint::connect_lazy(&endpoint);
    Ok(with_interceptor(
        channel,
        ApiInterceptor(
            config.token().await?,
            DefaultTimeout(DEFAULT_REQUEST_TIMEOUT),
        ),
    ))
}

/// Tries to use `connector` to create service connection. If this fails then asks the backend for
/// new service urls, update `SharedConfig` with them, and tries again.
pub async fn connect_with_discovery<'a, S, C, F>(
    config: &'a SharedConfig,
    mut connector: C,
) -> Result<S>
where
    C: FnMut(&'a SharedConfig) -> F,
    F: Future<Output = Result<S>> + 'a,
{
    if let Ok(service) = connector(config).await {
        Ok(service)
    } else {
        // if we can't connect - refresh urls and try again
        let services = {
            let mut client = ApiClient::build_with_default_connector(
                config,
                pb::discovery_service_client::DiscoveryServiceClient::with_interceptor,
            )
            .await?;
            api_with_retry!(
                client,
                client.services(pb::DiscoveryServiceServicesRequest {})
            )
            .with_context(|| "get service urls failed")?
        }
        .into_inner();
        config.set_mqtt_url(Some(services.notification_url)).await;
        connector(config).await
    }
}

pub struct ApiInterceptor(pub AuthToken, pub DefaultTimeout);

impl Interceptor for ApiInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        self.0
            .call(request)
            .and_then(|request: Request<()>| self.1.call(request))
    }
}

pub struct AuthToken(pub String);

impl Interceptor for AuthToken {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let token = &self.0;
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| Status::internal("Invalid authorization MetaData"))?,
        );
        Ok(request)
    }
}

impl AuthToken {
    pub fn expired(token: &str) -> Result<bool, Status> {
        let margin = chrono::Duration::minutes(1);
        Self::expiration(token).map(|exp| exp < chrono::Utc::now() - margin)
    }

    fn expiration(token: &str) -> Result<chrono::DateTime<chrono::Utc>, Status> {
        use base64::engine::general_purpose::STANDARD_NO_PAD;
        use chrono::TimeZone;

        #[derive(serde::Deserialize)]
        struct Field {
            exp: i64,
        }

        let unauth = |s| move || Status::unauthenticated(s);
        // Take the middle section of the jwt, which has the payload.
        let middle = token
            .split('.')
            .nth(1)
            .ok_or_else(unauth("Can't parse token"))?;
        // Base64 decode the payload.
        let decoded = STANDARD_NO_PAD
            .decode(middle)
            .ok()
            .ok_or_else(unauth("Token is not base64"))?;
        // Json-parse the payload, with only the `exp` field being of interest.
        let parsed: Field = serde_json::from_slice(&decoded)
            .ok()
            .ok_or_else(unauth("Token is not JSON with exp field"))?;
        // Now interpret this timestamp as an utc time.
        match chrono::Utc.timestamp_opt(parsed.exp, 0) {
            chrono::LocalResult::None => Err(unauth("Invalid timestamp")()),
            chrono::LocalResult::Single(expiration) => Ok(expiration),
            chrono::LocalResult::Ambiguous(expiration, _) => Ok(expiration),
        }
    }
}

pub type AuthenticatedService = InterceptedService<Channel, ApiInterceptor>;

#[async_trait]
pub trait ApiServiceConnector {
    async fn connect<T, I>(&self, with_interceptor: I) -> Result<T, Status>
    where
        I: Send + Sync + Fn(Channel, ApiInterceptor) -> T;
}

#[derive(Clone)]
pub struct DefaultConnector {
    pub config: SharedConfig,
}

#[async_trait]
impl ApiServiceConnector for DefaultConnector {
    async fn connect<T, I>(&self, with_interceptor: I) -> Result<T, Status>
    where
        I: Send + Sync + Fn(Channel, ApiInterceptor) -> T,
    {
        Ok(connect_to_api_service(&self.config, with_interceptor).await?)
    }
}
