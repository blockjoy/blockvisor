use crate::auth;
use crate::errors::AppError;
use crate::result::Result;
use anyhow::anyhow;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
    Argon2,
};
use authy::api::user;
use authy::Client;
use chrono::{DateTime, Utc};
use sendgrid::v3::{Content, Email, Message, Personalization, Sender};
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgRow, FromRow, PgConnection, PgPool, Row};
use std::{fmt, str::FromStr};
use uuid::Uuid;
use validator::Validate;

type AuthyUserApi = authy::User;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "enum_org_role", rename_all = "snake_case")]
pub enum UserOrgRole {
    Admin,
    Owner,
}

impl fmt::Display for UserOrgRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admin => write!(f, "admin"),
            Self::Owner => write!(f, "owner"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "enum_user_role", rename_all = "snake_case")]
pub enum UserRole {
    User,
    Host,
    Admin,
}

impl fmt::Display for UserRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admin => write!(f, "admin"),
            Self::Host => write!(f, "host"),
            Self::User => write!(f, "user"),
        }
    }
}

impl FromStr for UserRole {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "admin" => Ok(Self::Admin),
            "host" => Ok(Self::Host),
            _ => Ok(Self::User),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct RegistrationReq {
    pub first_name: String,
    pub last_name: String,
    #[validate(email)]
    pub email: String,
    pub organization: Option<String>,
    #[validate(length(min = 8), must_match = "password_confirm")]
    pub password: String,
    pub password_confirm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub orgs: Option<Vec<Org>>,
    #[serde(skip_serializing)]
    pub hashword: String,
    #[serde(skip_serializing)]
    pub salt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<PgRow> for User {
    fn from(row: PgRow) -> Self {
        User {
            id: row.try_get("id").expect("Couldn't try_get id for user."),
            first_name: row
                .try_get("first_name")
                .expect("Couldn't try_get first_name for user."),
            last_name: row
                .try_get("last_name")
                .expect("Couldn't try_get last_name for user."),
            email: row
                .try_get("email")
                .expect("Couldn't try_get email for user."),
            hashword: row
                .try_get("hashword")
                .expect("Couldn't try_get hashword for user."),
            salt: row
                .try_get("salt")
                .expect("Couldn't try_get salt for user."),
            token: row
                .try_get("token")
                .expect("Couldn't try_get token for user."),
            refresh: row
                .try_get("refresh")
                .expect("Couldn't try_get refresh for user."),
            orgs: None,
            created_at: row
                .try_get("created_at")
                .expect("Couldn't try_get created_at for user."),
            updated_at: row
                .try_get("updated_at")
                .expect("Couldn't try_get updated_at for user."),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct UserSummary {
    pub id: Uuid,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct UserLoginRequest {
    #[validate(email)]
    pub email: String,
    #[validate(length(min = 8))]
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRefreshRequest {
    pub refresh: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct PasswordResetRequest {
    #[validate(email)]
    pub email: String,
}

#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct PwdResetInfo {
    pub token: String,
    #[validate(length(min = 8), must_match = "password_confirm")]
    pub password: String,
    pub password_confirm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Org {
    pub id: Uuid,
    pub name: String,
    pub is_personal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_role: Option<UserOrgRole>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
impl User {
    pub async fn create_user(req: RegistrationReq, db_pool: &PgPool) -> Result<Self> {
        let _ = req
            .validate()
            .map_err(|e| AppError::ValidationError(e.to_string()));

        let argon2 = Argon2::default();
        let salt = SaltString::generate(&mut OsRng);
        if let Some(hashword) = argon2
            .hash_password(req.password.as_bytes(), salt.as_str())
            .map_err(|_| anyhow!("Hashing error"))?
            .hash
        {
            let mut tx = db_pool.begin().await?;
            let result = sqlx::query(
                r#"
                INSERT INTO
                users (email, hashword, salt,first_name,last_name)
                values
                (
                    Lower($1), $2, $3, $4, $5
                )
                   RETURNING *
                "#,
            )
            .bind(req.email)
            .bind(hashword.to_string())
            .bind(salt.as_str())
            .bind(req.first_name)
            .bind(req.last_name)
            .fetch_one(&mut tx)
            .await
            .map(Self::from)
            .map_err(AppError::from);

            let mut user = result.unwrap();
            let organization = req.organization.unwrap();
            let org = Org::find_by_name(&organization, &mut tx).await?;

            Org::create_orgs_users_owner(org.id, user.id, &mut tx).await?;

            user.orgs = Some(Org::find_all_by_user(user.id, &mut tx).await?);

            tx.commit().await?;
            Ok(user)
        } else {
            Err(AppError::ValidationError("Invalid password.".to_string()))
        }
    }

    pub async fn login(user_login_req: UserLoginRequest, db_pool: &PgPool) -> Result<User> {
        let user = User::find_by_email(&user_login_req.email, db_pool)
            .await?
            .set_jwt()
            .map_err(|_e| {
                AppError::InvalidAuthentication(anyhow!("Email or password is invalid."))
            })?;
        user.verify_password(&user_login_req.password)?;
        Ok(user)
    }

    pub async fn find_by_email(email: &str, db_pool: &PgPool) -> Result<User> {
        let mut tx = db_pool.begin().await?;
        let user = sqlx::query(
            r#"SELECT *
                    FROM   users
                    WHERE  Lower(email) = Lower($1)
                    LIMIT  1
                    "#,
        )
        .bind(email)
        .fetch_one(&mut tx)
        .await
        .map(Self::from)
        .map_err(AppError::from);
        let mut user = user.unwrap();
        user.orgs = Some(Org::find_all_by_user(user.id, &mut tx).await?);
        tx.commit().await?;
        user.set_jwt()
    }

    pub async fn find_summary_by_user(user_id: &Uuid, db_pool: &PgPool) -> Result<UserSummary> {
        let user = sqlx::query_as::<_, UserSummary>(
            r#"
            SELECT 
                users.id, 
                email
            FROM
                users
            WHERE
                users.id = $1
            "#,
        )
        .bind(user_id)
        .fetch_one(db_pool)
        .await?;

        Ok(user)
    }

    pub async fn find_by_id(id: Uuid, pool: &PgPool) -> Result<User> {
        let user = sqlx::query(r#"SELECT * FROM users WHERE id = $1 limit 1"#)
            .bind(id)
            .fetch_one(pool)
            .await
            .map(Self::from)
            .map_err(AppError::from);
        Ok(user.unwrap())
    }

    pub async fn refresh(req: UserRefreshRequest, pool: &PgPool) -> Result<User> {
        let user = User::find_by_refresh(&req.refresh, pool)
            .await?
            .set_jwt()
            .map_err(AppError::from)?;
        Ok(user)
    }
    pub async fn find_by_refresh(refresh: &str, pool: &PgPool) -> Result<User> {
        let user = sqlx::query(r#"SELECT * FROM users WHERE refresh = $1 limit 1"#)
            .bind(refresh)
            .fetch_one(pool)
            .await
            .map(Self::from)
            .map_err(AppError::from);
        Ok(user.unwrap())
    }

    pub async fn email_reset_password(req: PasswordResetRequest, db_pool: &PgPool) -> Result<()> {
        let user = User::find_by_email(&req.email, db_pool).await?;

        let auth_data = auth::UserAuthData {
            user_id: user.id,
            user_role: user.first_name.to_string(),
        };

        let token = auth::create_temp_jwt(&auth_data)?;

        let p = Personalization::new(Email::new(&user.email));

        let subject = "Reset Password".to_string();
        let body = format!(
            r##"
            <h1>Password Reset</h1>
            <p>You have requested to reset your BlockJoy password.
            Please visit <a href="https://console.blockjoy.com/reset?t={token}">
            https://console.blockjoy.com/reset?t={token}</a>.</p><br /><br /><p>Thank You!</p>"##
        );
        let sendgrid_api_key = dotenv::var("SENDGRID_API_KEY").map_err(|_| {
            AppError::UnexpectedError(anyhow!("Could not find SENDGRID_API_KEY in env."))
        })?;
        let sender = Sender::new(sendgrid_api_key);
        let m = Message::new(Email::new("BlockJoy <hello@blockjoy.com>"))
            .set_subject(&subject)
            .add_content(Content::new().set_content_type("text/html").set_value(body))
            .add_personalization(p);

        sender
            .send(&m)
            .await
            .map_err(|_| AppError::UnexpectedError(anyhow!("Could not send email")))?;

        Ok(())
    }

    pub async fn reset_password(req: &PwdResetInfo, db_pool: &PgPool) -> Result<User> {
        req.validate()
            .map_err(|e| AppError::ValidationError(e.to_string()))?;

        match auth::validate_jwt(&req.token)? {
            auth::JwtValidationStatus::Valid(auth_data) => {
                let user = User::find_by_id(auth_data.user_id, db_pool).await?;
                User::update_password(user, &req.password, db_pool).await
            }
            _ => Err(AppError::InsufficientPermissionsError),
        }
    }

    pub async fn update_password(user: User, password: &str, pool: &PgPool) -> Result<Self> {
        let argon2 = Argon2::default();
        let salt = SaltString::generate(&mut OsRng);
        if let Some(hashword) = argon2
            .hash_password(password.as_bytes(), salt.as_str())
            .map_err(|_| anyhow!("Hashing error"))?
            .hash
        {
            return sqlx::query(
                r#"
                UPDATE
                users
                SET
                hashword = $1,
                salt = $2
                WHERE
                  id = $3 RETURNING *, '' as orgs
                "#,
            )
            .bind(hashword.to_string())
            .bind(salt.as_str())
            .bind(user.id)
            .map(Self::from)
            .fetch_one(pool)
            .await
            .map_err(AppError::from)
            .unwrap()
            .set_jwt();
        }

        Err(AppError::ValidationError("Invalid password.".to_string()))
    }
    pub fn verify_password(&self, password: &str) -> Result<()> {
        let argon2 = Argon2::default();
        let parsed_hash = argon2
            .hash_password(password.as_bytes(), &self.salt)
            .map_err(|_| anyhow!("Hashing error"))?;

        if let Some(output) = parsed_hash.hash {
            if self.hashword == output.to_string() {
                return Ok(());
            }
        }
        Err(AppError::InvalidAuthentication(anyhow!(
            "Invalid email or password."
        )))
    }

    pub fn set_jwt(&mut self) -> Result<Self> {
        let auth_data = auth::UserAuthData {
            user_id: self.id,
            user_role: self.first_name.to_string(),
        };
        self.token = Some(auth::create_jwt(&auth_data)?);
        Ok(self.to_owned())
    }
}

impl Org {
    pub async fn find_by_name(name: &str, tx: &mut PgConnection) -> Result<Org> {
        sqlx::query_as::<_, Self>(
            "SELECT o.id, o.name, o.is_personal,o.created_at, o.updated_at , ou.role FROM orgs o inner join orgs_users ou on o.id = ou.orgs_id WHERE o.name = $1 order by created_at DESC",
        )
            .bind(name)
            .fetch_one(tx)
            .await
            .map_err(AppError::from)
    }

    pub async fn find_all_by_user(user_id: Uuid, tx: &mut PgConnection) -> Result<Vec<Self>> {
        sqlx::query_as::<_, Self>(
            "SELECT o.id, o.name, o.is_personal,o.created_at, o.updated_at , ou.role FROM orgs o inner join orgs_users ou on o.id = ou.orgs_id WHERE users_id = $1 order by created_at DESC",
        )
            .bind(user_id)
            .fetch_all(tx)
            .await
            .map_err(AppError::from)
    }

    pub async fn create_orgs_users_owner(
        org_id: Uuid,
        user_id: Uuid,
        tx: &mut PgConnection,
    ) -> Result<()> {
        let _result = sqlx::query(
            r#"
            INSERT INTO
            orgs_users (orgs_id, users_id, role)
            values
            (
                $1, $2, $3
            )
            "#,
        )
        .bind(org_id)
        .bind(user_id)
        .bind(UserOrgRole::Owner)
        .execute(tx)
        .await
        .map_err(AppError::from);

        Ok(())
    }
}

#[derive(Debug, Copy, Clone)]
pub struct AuthyUser;

impl AuthyUser {
    pub async fn register(
        client: &Client,
        authy_reg_req: &AuthyRegistrationReq,
    ) -> Result<AuthyIDReq> {
        let (_, user) = user::create(
            client,
            authy_reg_req.email.as_str(),
            authy_reg_req.country_code,
            authy_reg_req.phone.as_str(),
            false,
        )
        .unwrap();
        Ok(AuthyIDReq::new(user.id))
    }

    pub async fn qr(_client: &Client) -> Result<()> {
        unimplemented!()
    }

    pub async fn verify(client: &Client, authy_verify_req: &AuthyVerifyReq) -> Result<bool> {
        let mut user = AuthyUserApi::find(client, authy_verify_req.authy_id).unwrap();
        let result = user.verify(client, authy_verify_req.token.as_str());
        match result {
            Ok(verifies) => Ok(verifies),
            Err(_) => Err(AppError::ValidationError("Invalid token.".to_string())),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthyRegistrationReq {
    pub country_code: u16,
    pub phone: String,
    pub email: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Copy)]
pub struct AuthyIDReq {
    pub authy_id: u32,
}

impl AuthyIDReq {
    pub fn new(authy_id: u32) -> Self {
        AuthyIDReq { authy_id }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthyVerifyReq {
    pub authy_id: u32,
    pub token: String,
}
