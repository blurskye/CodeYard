//! Browser cookies and redirects for the shared OAuth operation.

use crate::backend::api::auth::errors;
use crate::backend::api::auth::session::LoginSessionRequest;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session;
use crate::backend::app::AppState;
use crate::backend::identity::oauth;
use crate::backend::identity::oauth::{providers, OAuthError};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

pub fn configured_public_origin() -> Option<String> {
    validated_public_origin(&std::env::var("PUBLIC_URL").ok()?)
}

pub(crate) fn validated_public_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(ApiError::new(code, message))).into_response()
}

/// `GET /api/auth/oauth/:provider`
fn failure(error: OAuthError, provider: &str) -> Response {
    let (status, code, message) = match error {
        OAuthError::UnknownProvider => (
            StatusCode::NOT_FOUND,
            "not_found",
            "Unknown OAuth provider".to_owned(),
        ),
        OAuthError::Disabled => (
            StatusCode::FORBIDDEN,
            "forbidden",
            format!("{provider} authentication is not enabled"),
        ),
        OAuthError::InvalidCode => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Invalid code or state".to_owned(),
        ),
        OAuthError::InvalidState => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Invalid, expired or transferred OAuth state".to_owned(),
        ),
        OAuthError::InvalidPkce => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Invalid PKCE state".to_owned(),
        ),
        OAuthError::SetupRequired => (
            StatusCode::PRECONDITION_REQUIRED,
            "setup_required",
            "Complete node setup before signing in".to_owned(),
        ),
        OAuthError::Admission(error) => return errors::admission_failure(error).into_response(),
        OAuthError::StateUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Authentication storage is temporarily unavailable".to_owned(),
        ),
        OAuthError::AccountUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Account storage is temporarily unavailable".to_owned(),
        ),
        OAuthError::ClientUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "oauth_error",
            "Provider client is unavailable".to_owned(),
        ),
        OAuthError::ProviderFailed => (
            StatusCode::BAD_GATEWAY,
            "oauth_error",
            "Provider authentication failed; start sign-in again".to_owned(),
        ),
        OAuthError::ApprovalRequired => (
            StatusCode::FORBIDDEN,
            "approval_required",
            "A private-node administrator must enroll this provider identity before sign-in"
                .to_owned(),
        ),
        OAuthError::Conflict => (
            StatusCode::CONFLICT,
            "conflict",
            "Could not allocate account identity".to_owned(),
        ),
        OAuthError::Denied => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Account is suspended or requires a password change".to_owned(),
        ),
        OAuthError::AccountAdmittedSessionFailed(error) => {
            let (status, body) = errors::session_issue_failure(error);
            return api_error(status, &body.0.error, "Account admission completed, but no session was issued. Check your session cookies and start sign-in again");
        }
    };
    api_error(status, code, &message)
}

pub async fn oauth_redirect(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
) -> Response {
    if providers::provider_meta(&provider).is_none() {
        return failure(OAuthError::UnknownProvider, &provider);
    }
    if providers::provider_credentials(&provider).is_none() {
        return failure(OAuthError::Disabled, &provider);
    }
    let Some(origin) = configured_public_origin() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "configuration_error",
            "Configure PUBLIC_URL with the public OAuth origin",
        );
    };
    // Always replace the browser nonce; only the latest flow in this browser completes.
    let nonce = session::generate_session_id();
    let token = session::generate_session_id();
    let verifier = if providers::supports_pkce(&provider) {
        session::generate_session_id()
    } else {
        String::new()
    };
    let redirect = format!("{origin}/api/auth/oauth/{provider}/callback");
    match oauth::begin(&state.pool, &provider, &redirect, &nonce, &token, &verifier).await {
        Ok(location) => (
            StatusCode::FOUND,
            [
                (header::LOCATION, location),
                (
                    header::SET_COOKIE,
                    session::build_oauth_browser_cookie(&nonce),
                ),
            ],
        )
            .into_response(),
        Err(error) => failure(error, &provider),
    }
}

#[derive(Debug, Deserialize)]
pub struct OAuthCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
}

pub async fn oauth_callback(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Query(query): Query<OAuthCallbackQuery>,
    headers: HeaderMap,
) -> Response {
    if providers::provider_meta(&provider).is_none() {
        return failure(OAuthError::UnknownProvider, &provider);
    }
    let (Some(code), Some(token)) = (query.code, query.state) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Missing code or state",
        );
    };
    if providers::provider_credentials(&provider).is_none() {
        return failure(OAuthError::Disabled, &provider);
    }
    if let Err(error) = oauth::validate_callback(&code, &token) {
        return failure(error, &provider);
    }
    // Preserve setup's precedence over missing browser binding.
    if let Err(error) = oauth::require_setup(&state.pool).await {
        return failure(error, &provider);
    }
    let Some(nonce) = session::random_cookie(&headers, session::oauth_browser_cookie_name()) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "OAuth must complete in its initiating browser",
        );
    };
    let Some(origin) = configured_public_origin() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "configuration_error",
            "Configure PUBLIC_URL with the public OAuth origin",
        );
    };
    let redirect = format!("{origin}/api/auth/oauth/{provider}/callback");
    let initiating = LoginSessionRequest::from_headers(&headers);
    match oauth::complete(
        &state.pool,
        oauth::Callback {
            provider: &provider,
            code: &code,
            token: &token,
            browser_nonce: &nonce,
            redirect: &redirect,
            session: initiating.context(),
        },
    )
    .await
    {
        Ok(authenticated) => (
            StatusCode::FOUND,
            [
                (
                    header::SET_COOKIE,
                    session::build_session_cookie(
                        &authenticated.session_id,
                        session::session_duration_days(),
                    ),
                ),
                (header::LOCATION, "/".to_owned()),
            ],
        )
            .into_response(),
        Err(error) => failure(error, &provider),
    }
}
