use crate::backend::api::auth::errors;
use crate::backend::api::auth::session::LoginSessionRequest;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session;
use crate::backend::api::user::responses::{AuthResponse, AuthUserResponse as UserResponse};
use crate::backend::app::AppState;
use crate::backend::identity;
use crate::backend::identity::credentials::PasswordAccount;
use crate::backend::identity::registration::RegistrationOutcome;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub email: Option<String>,
    pub password: String,
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let session = LoginSessionRequest::from_headers(&headers);
    let outcome = identity::registration::register_password(
        &state.pool,
        PasswordAccount {
            username: &req.username,
            email: req.email.as_deref(),
            password: &req.password,
        },
        session.context(),
    )
    .await
    .map_err(errors::registration_failure)?;
    match outcome {
        RegistrationOutcome::Requested => Ok((
            StatusCode::ACCEPTED,
            Json(AuthResponse {
                success: true,
                user: None,
                message: Some(
                    "Signup request submitted. An administrator must approve your access."
                        .to_owned(),
                ),
            }),
        )
            .into_response()),
        RegistrationOutcome::Registered(authenticated) => {
            let cookie = session::build_session_cookie(
                &authenticated.session_id,
                session::session_duration_days(),
            );
            Ok((
                StatusCode::CREATED,
                [(header::SET_COOKIE, cookie)],
                Json(AuthResponse {
                    success: true,
                    user: Some(UserResponse::from(authenticated.user)),
                    message: None,
                }),
            )
                .into_response())
        }
    }
}
