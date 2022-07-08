use crate::errors::AppError;
use crate::models::{User, UserRole};
use crate::result::Result;
use anyhow::anyhow;
use axum::{
    async_trait,
    extract::{FromRequest, RequestParts},
};
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::borrow::Cow;
use std::env;
use std::str::FromStr;
use uuid::Uuid;

const JWT_SECRET: &str = "?G'A$jNW<$6x(PdFP?4VdRvmotIV^^";

#[derive(Debug)]
pub struct UserAuthData {
    pub user_id: Uuid,
    pub user_role: String,
}
#[derive(Debug)]
pub enum JwtValidationStatus {
    Valid(UserAuthData),
    Expired(UserAuthData),
    Invalid,
}

#[derive(Debug, Deserialize, Serialize)]
struct Claims {
    sub: String,
    role: String,
    exp: i64,
}

/// Creates a jwt with default duration for expires.
pub fn create_jwt(data: &UserAuthData) -> Result<String> {
    let duration = chrono::Duration::days(1);
    create_jwt_with_duration(data, duration)
}

pub fn create_temp_jwt(data: &UserAuthData) -> Result<String> {
    let duration = chrono::Duration::seconds(60 * 12);
    create_jwt_with_duration(data, duration)
}

pub fn create_jwt_with_duration(data: &UserAuthData, duration: chrono::Duration) -> Result<String> {
    let exp = Utc::now()
        .checked_add_signed(duration)
        .expect("valid timestamp")
        .timestamp();

    let claims = Claims {
        sub: data.user_id.to_string(),
        role: data.user_role.clone(),
        exp,
    };

    let header = Header::new(Algorithm::HS512);
    Ok(encode(
        &header,
        &claims,
        &EncodingKey::from_secret(jwt_secret().as_bytes()),
    )?)
}

pub fn validate_jwt(jwt: &str) -> Result<JwtValidationStatus> {
    let validation = Validation {
        leeway: 60,
        validate_exp: false,
        algorithms: vec![Algorithm::HS512],
        ..jsonwebtoken::Validation::default()
    };

    let result = match decode::<Claims>(
        jwt,
        &DecodingKey::from_secret(jwt_secret().as_bytes()),
        &validation,
    ) {
        Ok(decoded) => {
            let user_id = Uuid::parse_str(&decoded.claims.sub)
                .map_err(|_| AppError::from(anyhow!("Error parsing uuid from JWT sub")))?;
            let user_role = decoded.claims.role;

            let auth_data = UserAuthData { user_id, user_role };

            // Check Expiration
            let exp = decoded.claims.exp;
            let now = get_current_timestamp();
            if exp < now - 60 {
                JwtValidationStatus::Expired(auth_data)
            } else {
                JwtValidationStatus::Valid(auth_data)
            }
        }

        Err(_) => JwtValidationStatus::Invalid,
    };

    Ok(result)
}

fn jwt_secret() -> String {
    env::var("JWT_SECRET").unwrap_or_else(|_| JWT_SECRET.to_string())
}

fn get_current_timestamp() -> i64 {
    Utc::now().timestamp()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Copy)]
pub struct UserAuthInfo {
    pub id: Uuid,
    pub role: UserRole,
}

pub type UserAuthToken = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Authentication {
    User(UserAuthInfo),
    Service(UserAuthToken),
    Fail(String),
}

impl Authentication {
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User(_))
    }
    pub fn is_admin(&self) -> bool {
        matches!(self, Self::User(u) if u.role == UserRole::Admin)
    }

    #[must_use]
    pub fn is_service(&self) -> bool {
        matches!(self, Self::Service(_))
    }

    /// Returns an error if not an admin
    pub fn try_admin(&self) -> Result<bool> {
        if self.is_admin() {
            Ok(true)
        } else {
            Err(AppError::InsufficientPermissionsError)
        }
    }

    /// Returns an error if not an admin
    pub fn try_service(&self) -> Result<bool> {
        if self.is_service() {
            Ok(true)
        } else {
            Err(AppError::InsufficientPermissionsError)
        }
    }

    /// Returns an error if user doesn't have access
    pub fn try_user_access(&self, user_id: Uuid) -> Result<bool> {
        match self {
            Self::User(u) if u.id == user_id => Ok(true),
            _ => Err(AppError::InsufficientPermissionsError),
        }
    }

    pub async fn get_user(&self, pool: PgPool) -> Result<User> {
        match self {
            Self::User(u) => User::find_by_id(u.id, &pool).await,
            _ => Err(AppError::InsufficientPermissionsError),
        }
    }
}

#[async_trait]
impl<B> FromRequest<B> for Authentication
where
    B: Send,
{
    type Rejection = AppError;

    async fn from_request(req: &mut RequestParts<B>) -> std::result::Result<Self, Self::Rejection> {
        if let Some(token) = req
            .headers()
            .get("Authorization")
            .and_then(|hv| hv.to_str().ok())
            .and_then(|hv| {
                let words = hv.split("Bearer").collect::<Vec<&str>>();
                let token = words.get(1).map(|w| w.trim());
                token.map(Cow::Borrowed)
            })
        {
            let api_service_secret =
                std::env::var("API_SERVICE_SECRET").unwrap_or_else(|_| "".into());
            let is_service_token = !api_service_secret.is_empty() && token == api_service_secret;
            if token.starts_with("eyJ") {
                debug!("JWT Auth in Bearer.");
                if let Ok(JwtValidationStatus::Valid(auth_data)) = validate_jwt(token.as_ref()) {
                    if let Ok(role) = UserRole::from_str(&auth_data.user_role) {
                        return Ok(Self::User(UserAuthInfo {
                            id: auth_data.user_id,
                            role,
                        }));
                    }
                }
            } else if is_service_token {
                debug!("Service Auth in Bearer.");
                return Ok(Self::Service(token.as_ref().to_string()));
            }
        };

        warn!(
            "Invalid token auth: {:?} ", //- {:?}",
            req.headers().get("Authorization"),
            //req.path()
        );
        Ok(Self::Fail("invalid authentication credentials".to_string()))
    }
}
