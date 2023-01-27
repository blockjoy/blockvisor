use anyhow::{anyhow, bail, Result};
use base64::Engine;
use jsonwebtoken as jwt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashMap;

pub trait Claim {}

#[derive(Serialize, Deserialize)]
pub struct AuthClaim {
    pub id: uuid::Uuid,
    pub exp: i64,
    pub token_type: String,
    pub role: String,
    pub data: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
pub struct RefreshClaim {
    pub id: uuid::Uuid,
    pub exp: i64,
    pub token_type: String,
}

#[derive(Serialize)]
pub struct RegistrationClaim {
    pub id: uuid::Uuid,
    pub exp: i64,
    pub token_type: String,
    pub role: String,
    pub email: String,
}

impl Claim for AuthClaim {}
impl Claim for RefreshClaim {}
impl Claim for RegistrationClaim {}

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

    pub fn create_register(id: uuid::Uuid, secret: String, email: String) -> String {
        let claim = RegistrationClaim {
            id,
            exp: 1768022101,
            token_type: "registration_confirmation".to_string(),
            role: "user".to_string(),
            email,
        };

        match Self::encode(&claim, &secret) {
            Ok(token) => base64::engine::general_purpose::STANDARD.encode(token),
            Err(_) => String::default(),
        }
    }

    pub fn create_auth(id: uuid::Uuid, secret: String, org_id: String, email: String) -> String {
        let claim = AuthClaim {
            id,
            exp: 1768022101,
            token_type: "user_auth".to_string(),
            role: "admin".to_string(),
            data: Some(HashMap::from([
                ("org_id".to_string(), org_id),
                ("org_role".to_string(), "owner".to_string()),
                ("email".to_string(), email),
            ])),
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

    pub fn from_encoded<T: Claim + DeserializeOwned>(secret: String, token: &str) -> Result<T> {
        let validation = jwt::Validation::new(jwt::Algorithm::HS512);
        let token = base64::engine::general_purpose::STANDARD.decode(token)?;
        let token = std::str::from_utf8(&token)?;

        match jwt::decode::<T>(
            token,
            &jwt::DecodingKey::from_secret(secret.as_bytes()),
            &validation,
        ) {
            Ok(token) => Ok(token.claims),
            Err(e) => bail!("Error decoding token: {e:?}"),
        }
    }
}
