use anyhow::{anyhow, bail, Result};
use jsonwebtoken as jwt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub struct AuthClaim {
    pub resource_type: String,
    pub resource_id: uuid::Uuid,
    pub iat: i64,
    pub exp: i64,
    pub endpoints: Vec<String>,
    pub data: HashMap<String, String>,
}

#[derive(Serialize)]
pub struct RefreshClaim {
    pub resource_id: uuid::Uuid,
    pub iat: i64,
    pub exp: i64,
}

pub struct TokenGenerator;

impl TokenGenerator {
    pub fn create_register(user_id: uuid::Uuid, secret: &str) -> String {
        let claim = AuthClaim {
            resource_type: "User".to_string(),
            resource_id: user_id,
            iat: 1668022101,
            exp: 1768022101,
            endpoints: vec!["AuthConfirm".to_string()],
            data: Default::default(),
        };

        match Self::encode(&claim, secret) {
            Ok(token) => token,
            Err(_) => String::default(),
        }
    }

    pub fn create_auth(user_id: uuid::Uuid, secret: &str) -> String {
        let claim = AuthClaim {
            resource_type: "User".to_string(),
            resource_id: user_id,
            iat: 1668022101,
            exp: 1768022101,
            endpoints: vec![
                "AuthRefresh".to_string(),
                "AuthUpdatePassword".to_string(),
                "BabelAll".to_string(),
                "BlockchainAll".to_string(),
                "CommandAll".to_string(),
                "DiscoveryAll".to_string(),
                "HostAll".to_string(),
                "HostProvisionAll".to_string(),
                "InvitationAll".to_string(),
                "KeyFileAll".to_string(),
                "MetricsAll".to_string(),
                "NodeAll".to_string(),
                "OrgAll".to_string(),
                "UserAll".to_string(),
            ],
            data: Default::default(),
        };

        match Self::encode(&claim, secret) {
            Ok(token) => token,
            Err(_) => String::default(),
        }
    }

    pub fn create_host(host_id: uuid::Uuid, secret: &str) -> String {
        let claim = AuthClaim {
            resource_type: "Host".to_string(),
            resource_id: host_id,
            iat: 1668022101,
            exp: 1768022101,
            endpoints: vec![
                "AuthRefresh".to_string(),
                "BabelAll".to_string(),
                "BlockchainAll".to_string(),
                "CommandAll".to_string(),
                "DiscoveryAll".to_string(),
                "HostAll".to_string(),
                "HostProvisionAll".to_string(),
                "InvitationAll".to_string(),
                "KeyFileAll".to_string(),
                "MetricsAll".to_string(),
                "NodeAll".to_string(),
                "OrgAll".to_string(),
                "UserAll".to_string(),
            ],
            data: Default::default(),
        };

        match Self::encode(&claim, secret) {
            Ok(token) => token,
            Err(_) => String::default(),
        }
    }

    fn encode<T: Serialize>(claim: &T, secret: &str) -> Result<String> {
        let headers = jwt::Header::new(jwt::Algorithm::HS512);
        let key = jwt::EncodingKey::from_secret(secret.as_ref());

        jwt::encode(&headers, &claim, &key).map_err(|e| anyhow!(e))
    }
}
