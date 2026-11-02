use super::error::ApiError;
use crate::backend::{crypto::is_hex_token, db};
use axum::{
    http::{header, HeaderMap, StatusCode},
    Json,
};
use rand::{rngs::OsRng, RngCore};

fn cookie_domain() -> Option<String> {
    std::env::var("COOKIE_DOMAIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_bool_env(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) fn secure_cookie() -> bool {
    // A pinned HTTPS deployment origin cannot be downgraded by an accidental
    // COOKIE_SECURE=false setting. Never infer transport from forwarded headers.
    if std::env::var("PUBLIC_URL")
        .ok()
        .and_then(|value| url::Url::parse(&value).ok())
        .is_some_and(|url| url.scheme() == "https")
    {
        return true;
    }
    std::env::var("COOKIE_SECURE")
        .map(|value| parse_bool_env(&value))
        .unwrap_or(false)
}

pub(crate) fn session_duration_days() -> i64 {
    std::env::var("SESSION_DURATION_DAYS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|days| (1..=365).contains(days))
        .unwrap_or(30)
}

pub(crate) fn generate_session_id() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub(crate) fn build_session_cookie(session_id: &str, max_age_days: i64) -> String {
    build_session_cookie_for(session_id, max_age_days.clamp(0, 365) * 24 * 60 * 60)
}

/// A session cookie that lasts `max_age_seconds`, for reissuing an existing session.
pub(crate) fn build_session_cookie_for(session_id: &str, max_age_seconds: i64) -> String {
    let mut cookie = format!(
        "session_id={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        session_id,
        max_age_seconds.max(0)
    );

    if let Some(domain) = cookie_domain() {
        cookie.push_str(&format!("; Domain={}", domain));
    }

    if secure_cookie() {
        cookie.push_str("; Secure");
    }

    cookie
}

pub(crate) fn build_logout_cookie() -> String {
    let mut cookie = "session_id=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string();

    if let Some(domain) = cookie_domain() {
        cookie.push_str(&format!("; Domain={}", domain));
    }

    if secure_cookie() {
        cookie.push_str("; Secure");
    }

    cookie
}

pub(crate) fn random_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for cookies in headers.get_all(header::COOKIE).iter() {
        for cookie in cookies.to_str().ok()?.split(';') {
            let Some((key, value)) = cookie.trim().split_once('=') else {
                continue;
            };
            if key != name {
                continue;
            }
            if found.is_some() || !is_hex_token(value, 64) {
                return None;
            }
            found = Some(value.to_owned());
        }
    }
    found
}

pub(crate) fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    random_cookie(headers, "session_id")
}

pub(crate) fn has_session_cookie(headers: &HeaderMap) -> bool {
    headers.get_all(header::COOKIE).iter().any(|header| {
        header.to_str().map_or(true, |cookies| {
            cookies
                .split(';')
                .any(|cookie| cookie.trim().starts_with("session_id="))
        })
    })
}

pub(crate) fn oauth_browser_cookie_name() -> &'static str {
    if std::env::var("PUBLIC_URL")
        .ok()
        .and_then(|value| url::Url::parse(&value).ok())
        .is_some_and(|url| url.scheme() == "http")
    {
        "oauth_browser"
    } else {
        "__Host-gitserv_oauth"
    }
}

pub(crate) fn build_oauth_browser_cookie(nonce: &str) -> String {
    let name = oauth_browser_cookie_name();
    // Host-only, Path=/ and Secure prevent same-site sibling cookie tossing on
    // HTTPS deployments. Only explicitly validated loopback HTTP uses the dev name.
    let mut cookie = format!("{name}={nonce}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600");
    if name.starts_with("__Host-") {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Set by `middleware::stamp_client_address` on every request; a value sent by the client is
/// dropped there.
pub(crate) const CLIENT_HEADER: &str = "x-codeyard-client";

/// The address the server saw for this request, used to limit failed sign-ins. `None` when the
/// request did not come in over a socket.
pub(crate) fn client_address(headers: &HeaderMap) -> Option<&str> {
    headers.get(CLIENT_HEADER)?.to_str().ok()
}

pub(crate) fn extract_user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(512).collect())
}

pub(crate) async fn get_current_user(
    pool: &db::DbPool,
    headers: &HeaderMap,
) -> Result<db::models::User, (StatusCode, Json<ApiError>)> {
    get_optional_user(pool, headers).await?.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiError::new(
                "unauthorized",
                "Session expired or not authenticated",
            )),
        )
    })
}

pub(crate) async fn get_optional_user(
    pool: &db::DbPool,
    headers: &HeaderMap,
) -> Result<Option<db::models::User>, (StatusCode, Json<ApiError>)> {
    let Some(session_id) = extract_session_id(headers) else {
        return Ok(None);
    };
    let user = db::users::sessions::get_session_account(pool, &session_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "session_unavailable",
                    "Session storage is temporarily unavailable",
                )),
            )
        })?;
    // Absent/expired/revoked is anonymous; a storage outage is NOT anonymous.
    let user = user.filter(optional_user_allowed);
    if user.as_ref().is_some_and(|user| user.must_change_password) {
        return Err((
            StatusCode::PRECONDITION_REQUIRED,
            Json(ApiError::new(
                "password_change_required",
                "Change your temporary password before accessing account resources",
            )),
        ));
    }
    Ok(user)
}

fn optional_user_allowed(user: &db::models::User) -> bool {
    !user.is_banned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parser_rejects_ambiguous_or_unbounded_identifiers() {
        let mut headers = HeaderMap::new();
        let id = generate_session_id();
        assert_eq!(id.len(), 64);
        headers.insert(
            header::COOKIE,
            format!("session_id={id}; unrelated=value").parse().unwrap(),
        );
        assert_eq!(extract_session_id(&headers), Some(id.clone()));
        headers.insert(header::COOKIE, "session_id=caller-picked".parse().unwrap());
        assert!(extract_session_id(&headers).is_none());
        headers.insert(
            header::COOKIE,
            format!("session_id={id}; session_id={id}").parse().unwrap(),
        );
        assert!(extract_session_id(&headers).is_none());
        headers.insert(header::COOKIE, format!("session_id={id}").parse().unwrap());
        headers.append(header::COOKIE, format!("session_id={id}").parse().unwrap());
        assert!(extract_session_id(&headers).is_none());
        assert!(has_session_cookie(&headers));
        let cookie = build_session_cookie(&id, i64::MAX);
        assert!(cookie.contains("Max-Age=31536000"));
    }

    #[test]
    fn optional_identity_rejects_banned_accounts_including_admins() {
        let mut user: db::models::User = serde_json::from_value(serde_json::json!({
            "id": 1,
            "username": "fixture",
            "is_admin": false,
            "is_banned": false,
            "must_change_password": false,
            "email_verified": false,
            "two_factor_enabled": false,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(optional_user_allowed(&user));
        user.is_banned = true;
        assert!(!optional_user_allowed(&user));
        user.is_admin = true;
        assert!(!optional_user_allowed(&user));
    }
}
