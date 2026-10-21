//! Browser cookies and redirects for the shared OAuth operation.

use crate::backend::api::auth::errors;
use crate::backend::api::auth::session::LoginSessionRequest;
use crate::backend::api::http::render::public_error;
use crate::backend::api::http::session;
use crate::backend::app::AppState;
use crate::backend::identity::oauth;
use crate::backend::identity::oauth::{providers, OAuthError};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

/// No request header determines a callback origin. HTTP is permitted only on loopback fixtures.
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

/// The provider sends the browser back here, so a failure is an error page, not JSON.
fn failure(error: OAuthError, provider: &str) -> Response {
    let (status, message) = match error {
        OAuthError::UnknownProvider => {
            (StatusCode::NOT_FOUND, "Unknown OAuth provider.".to_owned())
        }
        OAuthError::Disabled => (
            StatusCode::FORBIDDEN,
            format!("{provider} authentication is not enabled."),
        ),
        OAuthError::InvalidCode => (StatusCode::BAD_REQUEST, "Invalid code or state.".to_owned()),
        OAuthError::InvalidState => (
            StatusCode::BAD_REQUEST,
            "Invalid, expired or transferred OAuth state. Start signing in again.".to_owned(),
        ),
        OAuthError::InvalidPkce => (StatusCode::BAD_REQUEST, "Invalid PKCE state.".to_owned()),
        OAuthError::SetupRequired => (
            StatusCode::PRECONDITION_REQUIRED,
            "Complete node setup before signing in.".to_owned(),
        ),
        OAuthError::StateUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Authentication storage is temporarily unavailable.".to_owned(),
        ),
        OAuthError::AccountUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Account storage is temporarily unavailable.".to_owned(),
        ),
        OAuthError::ClientUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Provider client is unavailable.".to_owned(),
        ),
        OAuthError::ProviderFailed => (
            StatusCode::BAD_GATEWAY,
            "Provider authentication failed. Start signing in again.".to_owned(),
        ),
        OAuthError::ApprovalRequired => (
            StatusCode::FORBIDDEN,
            "A private-node administrator must enroll this provider identity before sign-in."
                .to_owned(),
        ),
        OAuthError::Conflict => (
            StatusCode::CONFLICT,
            "Could not allocate account identity.".to_owned(),
        ),
        OAuthError::Denied => (
            StatusCode::FORBIDDEN,
            "Account is suspended or requires a password change.".to_owned(),
        ),
        OAuthError::AccountAdmittedSessionFailed(error) => {
            let (status, _) = errors::session_issue_failure(error);
            return public_error(status, "Account admission completed, but no session was issued. Check your session cookies and start signing in again.");
        }
    };
    public_error(status, &message)
}

fn local_return_to(target: &str) -> &str {
    if crate::backend::api::http::origin::safe_redirect_path(target) {
        target
    } else {
        "/"
    }
}

pub async fn oauth_redirect(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if providers::provider_meta(&provider).is_none() {
        return failure(OAuthError::UnknownProvider, &provider);
    }
    if providers::provider_credentials(&provider).is_none() {
        return failure(OAuthError::Disabled, &provider);
    }
    let Some(origin) = configured_public_origin() else {
        return public_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Configure PUBLIC_URL with the public OAuth origin.",
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
    let return_to = local_return_to(query.get("redirect").map(String::as_str).unwrap_or("/"));
    match oauth::begin(
        &state.pool,
        &provider,
        &redirect,
        &nonce,
        &token,
        &verifier,
        return_to,
    )
    .await
    {
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
        return public_error(StatusCode::BAD_REQUEST, "Missing code or state.");
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
        return public_error(
            StatusCode::BAD_REQUEST,
            "OAuth must complete in its initiating browser.",
        );
    };
    let Some(origin) = configured_public_origin() else {
        return public_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Configure PUBLIC_URL with the public OAuth origin.",
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
        Ok((authenticated, return_to)) => (
            StatusCode::FOUND,
            [
                (
                    header::SET_COOKIE,
                    session::build_session_cookie(
                        &authenticated.session_id,
                        session::session_duration_days(),
                    ),
                ),
                (header::LOCATION, local_return_to(&return_to).to_owned()),
            ],
        )
            .into_response(),
        Err(error) => failure(error, &provider),
    }
}

#[cfg(test)]
mod return_tests {
    use super::*;

    #[test]
    fn oauth_returns_only_to_local_destinations() {
        assert_eq!(
            local_return_to("/alice/project/issues/7?tab=comments"),
            "/alice/project/issues/7?tab=comments"
        );
        for target in [
            "https://evil.test",
            "//evil.test",
            "/\\evil.test",
            "/%2fevil.test",
        ] {
            assert_eq!(local_return_to(target), "/");
        }
    }
}
