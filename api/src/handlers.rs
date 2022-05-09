use crate::auth::Authentication;
use crate::config::Twilio;
use crate::models::PasswordResetRequest;
use crate::models::PwdResetInfo;
use crate::models::RegistrationReq;
use crate::models::TwilioVerify;
use crate::models::User;
use crate::models::UserLoginRequest;
use crate::models::UserRefreshRequest;
use crate::result::Result;
use axum::{
    response::{IntoResponse, Response},
    Extension, Json,
};
use sqlx::PgPool;
use tracing::instrument;
use twilio_oai_verify_v2::apis::default_api::CreateChallengeParams;
use twilio_oai_verify_v2::apis::default_api::CreateNewFactorParams;
use twilio_oai_verify_v2::apis::default_api::UpdateFactorParams;
use uuid::Uuid;

type ApiResponse = Result<Response>;

/// GET handler for health requests by an application platform
///
/// Intended for use in environments such as Amazon ECS or Kubernetes which want
/// to validate that the HTTP service is available for traffic, by returning a
/// 200 OK response with any content.
#[allow(clippy::unused_async)]
pub async fn health() -> &'static str {
    "OK"
}

#[instrument(skip(db))]
pub async fn create_user(
    Json(user_req): Json<RegistrationReq>,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let result = User::create_user(user_req, &db).await;
    match result {
        Ok(summary) => Ok((Json(summary)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn user_summary(
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    auth: Authentication,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let _ = auth.try_admin()?;
    let result = User::find_summary_by_user(&id, &db).await;

    match result {
        Ok(summary) => Ok((Json(summary)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn login(
    Json(user_login_req): Json<UserLoginRequest>,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let result = User::login(user_login_req, &db).await;
    match result {
        Ok(user) => Ok((Json(user)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn whoami(
    Extension(db): Extension<PgPool>,
    //     auth: Authentication,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> ApiResponse {
    let result = User::find_by_id(id, &db).await;
    match result {
        Ok(user) => Ok((Json(user)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn refresh(
    Json(req): Json<UserRefreshRequest>,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let result = User::refresh(req, &db).await;
    match result {
        Ok(user) => Ok((Json(user)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn reset_pwd(
    Json(password_reset_req): Json<PasswordResetRequest>,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let result = User::email_reset_password(password_reset_req, &db).await;
    match result {
        Ok(_) => Ok(
            (Json("An email with reset instructions has been sent.".to_string())).into_response(),
        ),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn update_pwd(
    Json(password_reset_req): Json<PwdResetInfo>,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let result = User::reset_password(&password_reset_req, &db).await;
    match result {
        Ok(user) => Ok((Json(user)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn enable_two_factor(
    auth: Authentication,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let user = auth.get_user(&db).await?;
    let result = User::two_factor(&user.id, &db, true).await;
    match result {
        Ok(summary) => Ok((Json(summary)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn disable_two_factor(
    auth: Authentication,
    Extension(db): Extension<PgPool>,
) -> ApiResponse {
    let user = auth.get_user(&db).await?;
    let result = User::two_factor(&user.id, &db, false).await;
    match result {
        Ok(summary) => Ok((Json(summary)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn register_two_factor(
    auth: Authentication,
    Extension(db): Extension<PgPool>,
    Extension(twilio): Extension<Twilio>,
) -> ApiResponse {
    let user = auth.get_user(&db).await?;
    let mut params = CreateNewFactorParams::default();
    params.service_sid = twilio.service_id;
    params.identity = user.id.to_string();
    params.friendly_name = twilio.app_friendly_name;
    params.factor_type = String::from("totp");
    let result = TwilioVerify::register_oai_service(&twilio.config, params).await;
    match result {
        Ok(summary) => Ok((Json(summary)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn verify_two_factor_registration(
    auth: Authentication,
    Json(twilio_verify): Json<TwilioVerify>,
    Extension(db): Extension<PgPool>,
    Extension(twilio): Extension<Twilio>,
) -> ApiResponse {
    let user = auth.get_user(&db).await?;
    let mut params = UpdateFactorParams::default();
    params.service_sid = twilio.service_id;
    params.identity = user.id.to_string();
    params.sid = twilio_verify.factor_sid;
    params.auth_payload = Some(twilio_verify.token);
    let result = TwilioVerify::verify_oai_registration(&twilio.config, params).await;
    match result {
        Ok(summary) => Ok((Json(summary)).into_response()),
        Err(err) => Err(err),
    }
}

#[instrument(skip(db))]
pub async fn verify_two_factor(
    auth: Authentication,
    Json(twilio_verify): Json<TwilioVerify>,
    Extension(db): Extension<PgPool>,
    Extension(twilio): Extension<Twilio>,
) -> ApiResponse {
    let user = auth.get_user(&db).await?;
    let mut params = CreateChallengeParams::default();
    params.service_sid = twilio.service_id;
    params.identity = user.id.to_string();
    params.factor_sid = twilio_verify.factor_sid;
    params.auth_payload = Some(twilio_verify.token);
    let result = TwilioVerify::verify_oai(&twilio.config, params).await;
    match result {
        Ok(summary) => Ok((Json(summary)).into_response()),
        Err(err) => Err(err),
    }
}
