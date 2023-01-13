use anyhow::{anyhow, Result};
use base64::Engine;
use jsonwebtoken as jwt;
use serde::Serialize;

pub trait Claim {}

#[derive(Serialize)]
struct AuthClaim {
    id: uuid::Uuid,
    exp: i64,
    token_type: String,
    role: String,
}

#[derive(Serialize)]
struct RefreshClaim {
    id: uuid::Uuid,
    exp: i64,
    token_type: String,
}

impl Claim for AuthClaim {}
impl Claim for RefreshClaim {}

pub struct TokenGenerator;

impl TokenGenerator {
    pub fn create_refresh(id: uuid::Uuid, secret: String) -> String {
        let claim = RefreshClaim {
            id,
            exp: 1768022101,
            token_type: "user_refresh".to_string(),
        };

        Self::encode(&claim, &secret).unwrap()
    }

    pub fn create_auth(id: uuid::Uuid, secret: String) -> String {
        let claim = AuthClaim {
            id,
            exp: 1768022101,
            token_type: "user_auth".to_string(),
            role: "admin".to_string(),
        };

        match Self::encode(&claim, &secret) {
            Ok(token) => base64::engine::general_purpose::STANDARD.encode(token),
            Err(_) => String::default(),
        }
    }

    fn encode<T: Claim + Serialize>(claim: &T, secret: &String) -> Result<String> {
        let headers = jwt::Header::new(jwt::Algorithm::HS512);
        let key = jwt::EncodingKey::from_secret(secret.as_ref());

        jwt::encode(&headers, &claim, &key).map_err(|e| anyhow!(e))
    }
}
