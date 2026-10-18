use crate::backend::api::auth::errors;
use crate::backend::api::auth::session::LoginSessionRequest;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session;
use crate::backend::api::user::responses::{AuthResponse, AuthUserResponse as UserResponse};
use crate::backend::app::AppState;
use crate::backend::identity::credentials::PasswordAccount;
use crate::backend::identity::setup::SetupInput;
use crate::backend::{db, identity};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub email: Option<String>,
    pub password: String,
    pub node_mode: String,
    pub admin_secret: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub setup_completed: bool,
    pub node_mode: String,
}

pub(crate) fn configured_admin_secret() -> Option<String> {
    std::env::var("ADMIN_SECRET")
        .ok()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
}

pub(crate) async fn node_setup_status(
    pool: &db::DbPool,
) -> Result<SetupStatusResponse, (StatusCode, Json<ApiError>)> {
    let setup = identity::setup::status(pool)
        .await
        .map_err(|_| errors::database_failure())?;
    Ok(SetupStatusResponse {
        setup_completed: setup.setup_completed,
        node_mode: setup.node_mode,
    })
}

pub async fn setup_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SetupStatusResponse>, (StatusCode, Json<ApiError>)> {
    Ok(Json(node_setup_status(&state.pool).await?))
}

pub async fn setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<SetupRequest>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, Json<ApiError>)> {
    let session = LoginSessionRequest::from_headers(&headers);
    let secret = configured_admin_secret();
    let authenticated = identity::setup::complete_setup(
        &state.pool,
        SetupInput {
            account: PasswordAccount {
                username: &req.username,
                email: req.email.as_deref(),
                password: &req.password,
            },
            node_mode: &req.node_mode,
            admin_secret: req.admin_secret.as_deref(),
        },
        secret.as_deref(),
        session.context(),
    )
    .await
    .map_err(errors::setup_failure)?;
    let cookie =
        session::build_session_cookie(&authenticated.session_id, session::session_duration_days());
    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(AuthResponse {
            success: true,
            user: Some(UserResponse::from(authenticated.user)),
            message: Some("Node setup completed successfully".to_owned()),
        }),
    ))
}
