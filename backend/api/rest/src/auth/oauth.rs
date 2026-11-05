//! Browser cookies and redirects for the shared OAuth operation.
use axum::response::IntoResponse;

use crate::auth::session::LoginSessionRequest;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use codeyard_api_http::render::public_error;
use codeyard_app::app::AppState;
use codeyard_identity::oauth::OAuthError;
use codeyard_model::oauth_apps::validated_public_origin;
use serde::Deserialize;
use std::sync::Arc;

/// No request header determines a callback origin. HTTP is permitted only on loopback fixtures.
pub fn configured_public_origin() -> Option<String> {
    validated_public_origin(&std::env::var("PUBLIC_URL").ok()?)
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
            "Account or organization access is unavailable.".to_owned(),
        ),
        OAuthError::AccountAdmittedSessionFailed(error) => {
            let (status, _) = crate::auth::errors::session_issue_failure(error);
            return public_error(status, "Account admission completed, but no session was issued. Check your session cookies and start signing in again.");
        }
    };
    public_error(status, &message)
}

fn local_return_to(target: &str) -> &str {
    if codeyard_api_http::origin::safe_redirect_path(target) {
        target
    } else {
        "/"
    }
}

async fn enabled_provider(state: &AppState, provider: &str) -> Result<(), Response> {
    if let Some(configured) = codeyard_identity::oauth::oidc::provider(provider) {
        match codeyard_db_orgs::organizations::profiles::get_public_organization(
            &state.pool,
            &configured.organization,
        )
        .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(public_error(
                    StatusCode::NOT_FOUND,
                    "Organization not found.",
                ))
            }
            Err(_) => {
                return Err(public_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Organization unavailable.",
                ))
            }
        }
    }
    if codeyard_identity::oauth::providers::provider_credentials(provider).is_none() {
        return Err(failure(OAuthError::Disabled, provider));
    }
    Ok(())
}

pub async fn oauth_redirect(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !codeyard_identity::oauth::known_provider(&provider) {
        return failure(OAuthError::UnknownProvider, &provider);
    }
    if let Err(response) = enabled_provider(&state, &provider).await {
        return response;
    }
    let Some(origin) = configured_public_origin() else {
        return public_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Configure PUBLIC_URL with the public OAuth origin.",
        );
    };
    match codeyard_db_identity::auth_admission::admit_oauth_start(
        &state.pool,
        codeyard_identity::client_address::client_address(&headers),
    )
    .await
    {
        Ok(_) => {}
        Err(codeyard_db_identity::auth_admission::AdmissionError::Limited) => {
            return public_error(StatusCode::TOO_MANY_REQUESTS, "Too many sign-in attempts")
        }
        Err(codeyard_db_identity::auth_admission::AdmissionError::Unavailable) => {
            return public_error(StatusCode::SERVICE_UNAVAILABLE, "Sign-in unavailable")
        }
    }
    // Always replace the browser nonce; only the latest flow in this browser completes.
    let nonce = codeyard_api_http::session::generate_session_id();
    let token = match state
        .pool
        .cell_prefix()
        .cookie(&codeyard_api_http::session::generate_session_id())
    {
        Ok(token) => token,
        Err(_) => return failure(OAuthError::StateUnavailable, &provider),
    };
    let verifier = if codeyard_identity::oauth::providers::supports_pkce(&provider) {
        codeyard_api_http::session::generate_session_id()
    } else {
        String::new()
    };
    let redirect = format!("{origin}/login/oauth/{provider}/callback");
    let return_to = local_return_to(query.get("redirect").map(String::as_str).unwrap_or("/"));
    match codeyard_identity::oauth::begin(
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
                    codeyard_api_http::session::build_oauth_browser_cookie(&nonce),
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
    if !codeyard_identity::oauth::known_provider(&provider) {
        return failure(OAuthError::UnknownProvider, &provider);
    }
    let (Some(code), Some(token)) = (query.code, query.state) else {
        return public_error(StatusCode::BAD_REQUEST, "Missing code or state.");
    };
    if let Err(response) = enabled_provider(&state, &provider).await {
        return response;
    }
    if let Err(error) = codeyard_identity::oauth::validate_callback(&code, &token) {
        return failure(error, &provider);
    }
    // Preserve setup's precedence over missing browser binding.
    if let Err(error) = codeyard_identity::oauth::require_setup(&state.pool).await {
        return failure(error, &provider);
    }
    let Some(nonce) = codeyard_api_http::session::random_cookie(
        &headers,
        codeyard_api_http::session::oauth_browser_cookie_name(),
    ) else {
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
    let redirect = format!("{origin}/login/oauth/{provider}/callback");
    let initiating = LoginSessionRequest::from_headers(&headers);
    match codeyard_identity::oauth::complete(
        &state.pool,
        codeyard_identity::oauth::Callback {
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
        Ok((authenticated, return_to)) => {
            let cookie = match codeyard_api_http::session::build_session_cookie(
                &authenticated.session_id,
                codeyard_api_http::session::session_duration_days(),
                state.pool.cell_prefix(),
            ) {
                Ok(cookie) => cookie,
                Err(_) => return failure(OAuthError::StateUnavailable, &provider),
            };
            (
                StatusCode::FOUND,
                [
                    (header::SET_COOKIE, cookie),
                    (header::LOCATION, local_return_to(&return_to).to_owned()),
                ],
            )
                .into_response()
        }
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
