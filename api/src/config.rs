#![allow(clippy::module_name_repetitions)]

//! Strongly-typed configuration details for application behavior which may be
//! decided at runtime
//!
//! Runtime calculations are performed via the [`config`] crate. This includes
//! the ability to layer per-environment configuration files in TOML format as
//! well as just-in-time overrides via well-named environment variables.

use std::net::Ipv4Addr;

use config::{Config, ConfigError, Environment, File};
use dotenv::dotenv;
use secrecy::Secret;
use serde::Deserialize;
use twilio_oai_verify_v2::apis::configuration::Configuration;

/// The root configuration object, holding all available configuration details
/// as inner public fields
#[derive(Debug, Default, Deserialize)]
pub struct AppConfig {
    /// Configuration pertaining specifically to database connections,
    /// interactions, and authentication
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Configuration pertaining specifically to the app's exposed REST API
    #[serde(default)]
    pub http: HttpConfig,
    /// Configuration pertaining specifically to observability
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Configuration pertaining specifically to authy 2FA
    #[serde(default)]
    pub twilio_verify: TwilioVerifyConfig,
}

impl AppConfig {
    pub fn new() -> Result<Self, ConfigError> {
        dotenv().ok();
        let env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".into());

        Config::builder()
            .add_source(File::with_name("config/default").required(false))
            .add_source(File::with_name(&format!("config/{}", env)).required(false))
            .add_source(File::with_name("config/local").required(false))
            .add_source(Environment::default().try_parsing(true).separator("__"))
            .build()?
            .try_deserialize()
    }
}

/// Configuration pertaining specifically to database connections, interactions,
/// and authentication
///
/// Uses the [`secrecy`] crate to help protect sensitive values from being
/// leaked to log output, stacktraces, etc.
#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub max_lifetime_seconds: u64,
    #[serde(default = "default_database_url")]
    pub url: Secret<String>,
}

fn default_database_url() -> Secret<String> {
    Secret::new("postgresql://postgres:postgres@localhost/blockvisor_dev".to_owned())
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            max_connections: 128,
            // TODO: number of CPU cores/worker threads
            min_connections: 8,
            connect_timeout_seconds: 30,
            idle_timeout_seconds: 900,
            max_lifetime_seconds: 3600,
            url: default_database_url(),
        }
    }
}

/// Configuration pertaining specifically to the app's exposed REST API
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct HttpConfig {
    /// The default IPv4 address to bind the application to, defaulting to `0.0.0.0`
    #[serde(default = "default_listen_address")]
    pub listen_address: Ipv4Addr,
    /// The default TCP port to bind the application to, defaulting to `8080`
    #[serde(default = "default_port")]
    pub listen_port: u16,
}

fn default_listen_address() -> Ipv4Addr {
    Ipv4Addr::new(0, 0, 0, 0)
}

fn default_port() -> u16 {
    8080
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen_address: "0.0.0.0".parse().unwrap(),
            listen_port: 8080,
        }
    }
}

/// Available, named presets for logging style, corresponding closely to
/// [`mod@tracing_subscriber::fmt`]'s available choices.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Full,
    Pretty,
    Json,
}

impl Default for LogFormat {
    fn default() -> Self {
        Self::Full
    }
}

/// Configuration pertaining specifically to observability
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TelemetryConfig {
    /// Should an [`opentelemetry`] stack be initialized with the application as an active [`tracing`] subscriber/layer?
    #[serde(default)]
    pub opentelemetry: bool,
    /// A remote endpoint to report to when `opentelemetry` field is set to true
    /// - assumes a gRPC listener compatible with OTLP protocol, see
    /// [`crate::telemetry`] for details
    #[serde(default)]
    pub opentelemetry_endpoint: Option<String>,
    /// Select a named logging preset from [`LogFormat`]
    #[serde(default)]
    pub log_format: LogFormat,
}

/// Configuration pertaining specifically to Twilio Verify 2FA
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TwilioVerifyConfig {
    /// Twilio Account Key
    #[serde(default)]
    pub twilio_account_sid: String,
    /// Twilio Account Auth Token
    #[serde(default)]
    pub twilio_auth_token: String,
    /// Service ID of Twilio Verify application you created
    #[serde(default)]
    pub service_id: String,
    /// App User Friendly name to appear on authenticator application eg(Authy, Google Authenticator, etc.)
    #[serde(default)]
    pub app_friendly_name: String,
}

#[derive(Clone, Debug, Default)]
pub struct Twilio {
    pub config: Configuration,
    pub service_id: String,
    pub app_friendly_name: String,
}
impl Twilio {
    pub fn new(config: Configuration, service_id: String, app_friendly_name: String) -> Self {
        Twilio {
            config,
            service_id,
            app_friendly_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn test_env_prefix() {
        std::env::set_var("DATABASE__URL", "postgres://");
        std::env::set_var("TELEMETRY__OPENTELEMETRY", "true");
        std::env::set_var("HTTP__LISTEN_PORT", "3000");
        std::env::set_var("TELEMETRY__LOG_FORMAT", "json");
        let config = AppConfig::new().unwrap();

        assert_eq!(config.database.url.expose_secret(), "postgres://");
        assert!(config.telemetry.opentelemetry);
        assert_eq!(config.http.listen_port, 3000);
        assert_eq!(config.telemetry.log_format, LogFormat::Json);

        std::env::remove_var("DATABASE__URL");
        std::env::remove_var("TELEMETRY__OPENTELEMETRY");
        std::env::remove_var("HTTP__LISTEN_PORT");
        std::env::remove_var("TELEMETRY__LOG_FORMAT");
    }
}
