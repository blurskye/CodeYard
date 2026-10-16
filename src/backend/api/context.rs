//! Verified browser request context, setup admission and form protection.
use super::origin::same_origin_submission;
use super::render::{public_error, redirect_response};
use super::session;
use crate::{
    backend::{
        app::AppState,
        db::{self, models::User},
    },
    webui::document::{Chrome, Theme},
};
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};
use subtle::ConstantTimeEq;

const CSRF_TTL_SECONDS: i64 = 2 * 60 * 60;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExistingCsrfError {
    Missing,
    Unavailable,
}

async fn csrf_token_for_session(
    state: &AppState,
    browser_session: &db::models::BrowserSession,
) -> anyhow::Result<Option<String>> {
    let candidate = session::generate_session_id();
    let ttl = CSRF_TTL_SECONDS
        .min((browser_session.expires_at - chrono::Utc::now()).num_seconds())
        .max(1);
    db::users::sessions::get_or_create_csrf_token(
        &state.pool,
        &browser_session.id,
        browser_session.user_id,
        &candidate,
        ttl,
    )
    .await
}

pub(crate) async fn csrf_token_for_existing_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, ExistingCsrfError> {
    let id = session::extract_session_id(headers)
        .filter(|id| valid_random_token(id))
        .ok_or(ExistingCsrfError::Missing)?;
    let session = db::users::sessions::get_browser_session(&state.pool, &id)
        .await
        .map_err(|_| ExistingCsrfError::Unavailable)?
        .ok_or(ExistingCsrfError::Missing)?;
    csrf_token_for_session(state, &session)
        .await
        .map_err(|_| ExistingCsrfError::Unavailable)?
        .filter(|token| valid_random_token(token))
        .ok_or(ExistingCsrfError::Missing)
}

pub struct PageContext {
    pub state: Arc<AppState>,
    pub headers: HeaderMap,
    pub user: Option<User>,
    pub csrf_token: String,
    pub path: String,
    pub query: HashMap<String, String>,
    /// The GET page that the appearance switch returns to. A form submission renders its
    /// page at the form's own URL, which accepts only POST, so the form handler sets this.
    pub return_to: Option<String>,
    pending_session_cookie: Option<String>,
    had_verified_session: bool,
    pub(super) location: String,
}

impl PageContext {
    pub async fn load(
        state: Arc<AppState>,
        headers: HeaderMap,
        uri: &Uri,
    ) -> Result<Self, Response> {
        let unavailable = || {
            public_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The service is temporarily unavailable. Please try again.",
            )
        };
        let config = db::node_config::get_node_configuration(&state.pool)
            .await
            .map_err(|_| unavailable())?;
        let setup_completed = config.as_ref().is_some_and(|c| c.setup_completed);
        let private_mode = config
            .as_ref()
            .is_some_and(|c| c.node_mode.eq_ignore_ascii_case("private"));

        // Do not treat a failed lookup as anonymous, or a missing setup row as an outage.
        let supplied_id = session::extract_session_id(&headers).filter(|id| valid_random_token(id));
        let existing_session = match supplied_id.as_deref() {
            Some(id) => db::users::sessions::get_browser_session(&state.pool, id)
                .await
                .map_err(|_| unavailable())?,
            None => None,
        };
        let user = match existing_session.as_ref().and_then(|s| s.user_id) {
            Some(id) => {
                let user = db::users::accounts::get_user_by_id(&state.pool, id)
                    .await
                    .map_err(|_| unavailable())?
                    .ok_or_else(|| {
                        public_error(
                            StatusCode::UNAUTHORIZED,
                            "Your account is no longer available. Sign in again.",
                        )
                    })?;
                if user.is_banned {
                    return Err(public_error(
                        StatusCode::FORBIDDEN,
                        "This account is suspended.",
                    ));
                }
                Some(user)
            }
            None => None,
        };
        let path = uri.path().to_string();
        if !setup_completed && !matches!(path.as_str(), "/setup" | "/_forms/theme") {
            return Err(redirect_response("/setup"));
        }
        if setup_completed && private_mode && user.is_none() && !public_identity_path(&path) {
            return Err(redirect_response("/enter"));
        }
        // A temporary password may only reach the settings page, its password form,
        // sign-out and the theme switch.
        if user.as_ref().is_some_and(|u| u.must_change_password)
            && !matches!(
                path.as_str(),
                "/settings" | "/settings/password" | "/logout" | "/_forms/theme"
            )
        {
            return Err(if path.starts_with("/settings/") {
                public_error(
                    StatusCode::FORBIDDEN,
                    "Change your temporary password before using other settings.",
                )
            } else {
                redirect_response("/settings")
            });
        }

        let had_verified_session = existing_session.is_some();
        let (verified_session, pending_session_cookie) = match existing_session {
            Some(s) => (s, None),
            None => {
                // Never adopt an attacker-supplied unknown cookie ID.
                let id = session::generate_session_id();
                let agent = session::extract_user_agent(&headers);
                let s = db::users::sessions::create_browser_session(
                    &state.pool,
                    &id,
                    agent.as_deref(),
                    None,
                    chrono::Utc::now() + chrono::Duration::days(session::session_duration_days()),
                )
                .await
                .map_err(|_| unavailable())?;
                (
                    s,
                    Some(session::build_session_cookie(
                        &id,
                        session::session_duration_days(),
                    )),
                )
            }
        };
        // PostgreSQL owns the session and its synchronizer together. Concurrent
        // first GETs share a token; login rotation invalidates the old identity.
        let csrf_token = csrf_token_for_session(&state, &verified_session)
            .await
            .map_err(|_| unavailable())?
            .filter(|token| valid_random_token(token))
            .ok_or_else(unavailable)?;
        let query = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
        Ok(Self {
            state,
            headers,
            user,
            csrf_token,
            path,
            query,
            return_to: None,
            pending_session_cookie,
            had_verified_session,
            location: uri
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/")
                .to_string(),
        })
    }

    #[expect(
        clippy::result_large_err,
        reason = "The shared HTML helper contract returns complete Axum responses without boxing."
    )]
    pub fn require_user(&self) -> Result<&User, Response> {
        self.user.as_ref().ok_or_else(|| self.redirect("/login"))
    }

    #[expect(
        clippy::result_large_err,
        reason = "The shared HTML helper contract returns complete Axum responses without boxing."
    )]
    pub fn check_csrf(&self, form: &HashMap<String, String>) -> Result<(), Response> {
        if !self.had_verified_session
            || !csrf_matches(&self.csrf_token, form.get("csrf_token").map(String::as_str))
            || !same_origin_submission(&self.headers)
        {
            return Err(self.error(
                StatusCode::FORBIDDEN,
                "The form expired or came from another site. Reload the page and try again.",
            ));
        }
        Ok(())
    }

    pub(super) fn chrome(&self) -> Chrome {
        Chrome {
            username: self.user.as_ref().map(|u| u.username.clone()),
            is_admin: self.user.as_ref().is_some_and(|u| u.is_admin),
            csrf_token: self.csrf_token.clone(),
            return_to: self.return_to.clone().unwrap_or_else(|| {
                let mut pairs: Vec<_> = self.query.iter().collect();
                pairs.sort_by(|a, b| a.0.cmp(b.0));
                let query = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(pairs)
                    .finish();
                if query.is_empty() {
                    self.path.clone()
                } else {
                    format!("{}?{}", self.path, query)
                }
            }),
            theme: cookie_value(&self.headers, "theme")
                .and_then(Theme::parse)
                .unwrap_or_default(),
        }
    }

    pub(crate) fn with_cookie(&self, mut response: Response) -> Response {
        if let Some(cookie) = &self.pending_session_cookie {
            if let Ok(value) = HeaderValue::from_str(cookie) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
        response
    }
}

fn valid_random_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}
pub(crate) fn csrf_matches(expected: &str, supplied: Option<&str>) -> bool {
    supplied.is_some_and(|token| {
        valid_random_token(token) && bool::from(expected.as_bytes().ct_eq(token.as_bytes()))
    })
}
fn public_identity_path(path: &str) -> bool {
    matches!(
        path,
        "/enter" | "/login" | "/register" | "/setup" | "/logout" | "/_forms/theme"
    )
}
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|v| v.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::origin::safe_redirect_path;
    use crate::backend::api::render::{accepts_page_model, error_model_response};
    #[tokio::test]
    async fn forms_require_a_preexisting_verified_session_and_preserve_cookie() {
        let mut ctx = PageContext {
            state: Arc::new(crate::backend::api::ssr_router_tests::closed_state().await),
            headers: HeaderMap::new(),
            user: None,
            csrf_token: "a".repeat(64),
            path: "/setup".into(),
            query: HashMap::new(),
            return_to: None,
            pending_session_cookie: Some(
                "session_id=fixture; Path=/; HttpOnly; SameSite=Lax".into(),
            ),
            had_verified_session: false,
            location: "/setup".into(),
        };
        let form = HashMap::from([("csrf_token".into(), ctx.csrf_token.clone())]);
        assert_eq!(
            ctx.check_csrf(&form).unwrap_err().status(),
            StatusCode::FORBIDDEN
        );
        ctx.had_verified_session = true;
        assert!(ctx.check_csrf(&form).is_ok());
        ctx.headers
            .insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert_eq!(
            ctx.check_csrf(&form).unwrap_err().status(),
            StatusCode::FORBIDDEN
        );
        let response = ctx.redirect("/setup");
        assert!(response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("HttpOnly; SameSite=Lax"));
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        ctx.headers.remove("sec-fetch-site");
        assert!(ctx.check_csrf(&HashMap::new()).is_err());
        ctx.csrf_token = "b".repeat(64); // expired or identity-transition token
        assert!(ctx.check_csrf(&form).is_err());
    }

    #[tokio::test]
    async fn negotiated_page_models_are_authorized_presentation_not_private_user_rows() {
        let now = chrono::Utc::now();
        let user = User {
            id: 10,
            username: "alice".into(),
            email: Some("private@example.test".into()),
            password_hash: Some("PRIVATE_HASH_SENTINEL".into()),
            avatar_file_id: None,
            display_name: Some("Alice".into()),
            bio: None,
            location: None,
            website: None,
            is_admin: false,
            is_banned: false,
            must_change_password: false,
            email_verified: false,
            two_factor_enabled: false,
            created_at: now,
            updated_at: now,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static(crate::webui::bootstrap::MODEL_CONTENT_TYPE),
        );
        let ctx = PageContext {
            state: Arc::new(crate::backend::api::ssr_router_tests::closed_state().await),
            headers,
            user: Some(user),
            csrf_token: "a".repeat(64),
            path: "/search".into(),
            query: HashMap::from([("q".into(), "rust".into())]),
            return_to: None,
            pending_session_cookie: None,
            had_verified_session: true,
            location: "/search?q=rust".into(),
        };
        let response = ctx.page_model(
            "Search",
            crate::webui::app::Page::Error {
                status: 404,
                message: "Typed test view".into(),
                repo: None,
                signed_in: true,
            },
        );
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            crate::webui::bootstrap::MODEL_CONTENT_TYPE
        );
        assert_eq!(response.headers()[header::VARY], "Accept, Cookie");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let json = String::from_utf8(body.to_vec()).unwrap();
        assert!(!json.contains("PRIVATE_HASH_SENTINEL"));
        assert!(!json.contains("private@example.test"));
        assert!(!json.contains("password_hash"));
        let model: crate::webui::app::PageEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(model.chrome.username.as_deref(), Some("alice"));
        assert_eq!(model.location, "/search?q=rust");
    }

    #[test]
    fn explicit_navigation_accept_is_required() {
        let mut headers = HeaderMap::new();
        assert!(!accepts_page_model(&headers));
        headers.insert(header::ACCEPT, HeaderValue::from_static("text/html, */*"));
        assert!(!accepts_page_model(&headers));
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static(crate::webui::bootstrap::MODEL_CONTENT_TYPE),
        );
        assert!(accepts_page_model(&headers));
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/vnd.codeyard.page+json;q=0"),
        );
        assert!(!accepts_page_model(&headers));
    }

    #[test]
    fn csrf_requires_the_exact_random_token() {
        let token = "a".repeat(64);
        assert!(csrf_matches(&token, Some(&token)));
        assert!(!csrf_matches(&token, None));
        assert!(!csrf_matches(&token, Some(&"b".repeat(64))));
        assert!(!csrf_matches(&token, Some("short")));
    }
    #[test]
    fn redirects_reject_external_encoded_and_header_injection_targets() {
        for path in [
            "https://evil.test",
            "//evil.test",
            "/\\evil.test",
            "/%2Fevil.test",
            "/%5cevil",
            "/x%0d%0aLocation:y",
            "/\nLocation:y",
        ] {
            assert!(!safe_redirect_path(path), "{path}");
        }
        for path in [
            "/",
            "/alice/repo?tab=files",
            "/search?q=hello%20world",
            "/search?q=a%5Cb",
            "/alice/repo/tree/feature%2Fbranch",
        ] {
            assert!(safe_redirect_path(path));
        }
        assert_eq!(
            redirect_response("//evil.test").headers()[header::LOCATION],
            "/"
        );
        assert_eq!(
            redirect_response("/settings").status(),
            StatusCode::SEE_OTHER
        );
    }
    #[test]
    fn cross_site_metadata_and_origins_fail_closed() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("node.test"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.test"),
        );
        assert!(!same_origin_submission(&headers));
        headers.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!same_origin_submission(&headers));
        headers.remove(header::ORIGIN);
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin_submission(&headers));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-site"));
        assert!(!same_origin_submission(&headers));
    }
    #[tokio::test]
    async fn negotiated_error_preserves_status_exact_location_and_safe_headers() {
        let location = "/missing?q=a%26b&tag=one&tag=two&empty=";
        let page = crate::webui::app::Page::Error {
            status: 404,
            message: "Not found".into(),
            repo: None,
            signed_in: false,
        };
        let response = error_model_response(
            StatusCode::NOT_FOUND,
            page,
            location.into(),
            Chrome::default(),
        )
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            crate::webui::bootstrap::MODEL_CONTENT_TYPE
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(response.headers()[header::VARY], "Accept, Cookie");
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let envelope: crate::webui::app::PageEnvelope = serde_json::from_slice(&body).unwrap();
        assert_eq!(envelope.location, location);
        assert!(matches!(
            envelope.page,
            crate::webui::app::Page::Error { status: 404, .. }
        ));
    }

    #[tokio::test]
    async fn personalized_error_has_real_status_safe_headers_and_escaped_body() {
        let response = public_error(StatusCode::SERVICE_UNAVAILABLE, "<private>");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(response.headers()[header::VARY], "Accept, Cookie");
        assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("script-src 'self' 'wasm-unsafe-eval'"));
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("&#60;private&#62;"));
        assert!(!html.contains("<script"));
    }
}
