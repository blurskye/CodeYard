use crate::error::ApiError;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_support::cells::{Prefix, Secret};
use codeyard_support::crypto::is_hex_token;

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

pub fn secure_cookie() -> bool {
    // A pinned HTTPS deployment origin cannot be downgraded by an accidental
    // COOKIE_SECURE=false setting. Never infer transport from forwarded headers.
    if codeyard_model::http_config::public_url()
        .ok()
        .flatten()
        .is_some_and(|url| url.scheme() == "https")
    {
        return true;
    }
    std::env::var("COOKIE_SECURE")
        .map(|value| parse_bool_env(&value))
        .unwrap_or(false)
}

pub fn session_duration_days() -> i64 {
    std::env::var("SESSION_DURATION_DAYS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|days| (1..=365).contains(days))
        .unwrap_or(30)
}

pub fn generate_session_id() -> String {
    codeyard_support::crypto::random_hex()
}

pub fn build_session_cookie(
    session_id: &str,
    max_age_days: i64,
    cell: Prefix,
) -> Result<String, codeyard_support::cells::Invalid> {
    build_session_cookie_for(session_id, max_age_days.clamp(0, 365) * 24 * 60 * 60, cell)
}

/// A session cookie that lasts `max_age_seconds`, for reissuing an existing session.
pub(crate) fn build_session_cookie_for(
    session_id: &str,
    max_age_seconds: i64,
    cell: Prefix,
) -> Result<String, codeyard_support::cells::Invalid> {
    let session_id = cell.cookie(session_id)?;
    let name = session_cookie_name();
    let mut cookie = format!(
        "{name}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        session_id,
        max_age_seconds.max(0)
    );

    if let Some(domain) = cookie_domain() {
        cookie.push_str(&format!("; Domain={}", domain));
    }

    if secure_cookie() {
        cookie.push_str("; Secure");
    }

    Ok(cookie)
}

pub fn build_logout_cookie() -> String {
    let mut cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        session_cookie_name()
    );

    if let Some(domain) = cookie_domain() {
        cookie.push_str(&format!("; Domain={}", domain));
    }

    if secure_cookie() {
        cookie.push_str("; Secure");
    }

    cookie
}

fn named_cookie(headers: &HeaderMap, name: &str, limit: usize) -> Option<String> {
    let mut found = None;
    for cookies in headers.get_all(header::COOKIE).iter() {
        for cookie in cookies.to_str().ok()?.split(';') {
            let Some((key, value)) = cookie.trim().split_once('=') else {
                continue;
            };
            if key != name {
                continue;
            }
            if found.is_some() || value.len() > limit {
                return None;
            }
            found = Some(value);
        }
    }
    found.map(str::to_owned)
}

pub fn random_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    named_cookie(headers, name, 64).filter(|value| is_hex_token(value, 64))
}

pub(crate) fn session_cookie_name() -> &'static str {
    if secure_cookie() && cookie_domain().is_none() {
        "__Host-session_id"
    } else {
        "session_id"
    }
}

pub fn extract_session(headers: &HeaderMap) -> Option<(Prefix, String)> {
    let value = named_cookie(headers, session_cookie_name(), 70)?;
    let parsed = Secret::cookie(&value).ok()?;
    Some((parsed.cell, parsed.secret.to_owned()))
}

pub fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    extract_session(headers).map(|(_, secret)| secret)
}

pub fn extract_local_session_id(
    pool: &codeyard_db_core::DbPool,
    headers: &HeaderMap,
) -> Option<String> {
    extract_session(headers)
        .filter(|(cell, _)| *cell == pool.cell_prefix())
        .map(|(_, secret)| secret)
}

pub fn has_session_cookie(headers: &HeaderMap) -> bool {
    headers.get_all(header::COOKIE).iter().any(|header| {
        header.to_str().map_or(true, |cookies| {
            cookies.split(';').any(|cookie| {
                cookie
                    .trim()
                    .split_once('=')
                    .is_some_and(|(name, _)| name == session_cookie_name())
            })
        })
    })
}

pub fn oauth_browser_cookie_name() -> &'static str {
    if codeyard_model::http_config::public_url()
        .ok()
        .flatten()
        .is_some_and(|url| url.scheme() == "http")
    {
        "oauth_browser"
    } else {
        "__Host-gitserv_oauth"
    }
}

pub fn build_oauth_browser_cookie(nonce: &str) -> String {
    let name = oauth_browser_cookie_name();
    // Host-only, Path=/ and Secure prevent same-site sibling cookie tossing on
    // HTTPS deployments. Only explicitly validated loopback HTTP uses the dev name.
    let mut cookie = format!("{name}={nonce}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600");
    if name.starts_with("__Host-") {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn extract_user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(512).collect())
}

pub async fn get_current_user(
    pool: &codeyard_db_core::DbPool,
    headers: &HeaderMap,
) -> Result<codeyard_db_core::models::User, (StatusCode, Json<ApiError>)> {
    get_optional_user(pool, headers).await?.ok_or_else(|| {
        crate::shared::auth::errors::admin_authority_failure(
            codeyard_identity::sessions::AccountAuthorityError::NotAuthenticated,
        )
    })
}

pub async fn get_optional_user(
    pool: &codeyard_db_core::DbPool,
    headers: &HeaderMap,
) -> Result<Option<codeyard_db_core::models::User>, (StatusCode, Json<ApiError>)> {
    let Some(session_id) = extract_local_session_id(pool, headers) else {
        return Ok(None);
    };
    let user = codeyard_db_identity::users::sessions::get_session_account(pool, &session_id)
        .await
        .map_err(|_| crate::shared::auth::errors::session_failure())?;
    // Absent/expired/revoked is anonymous; a storage outage is NOT anonymous.
    let user = user.filter(optional_user_allowed);
    if user.as_ref().is_some_and(|user| user.must_change_password) {
        return Err(crate::shared::auth::errors::admin_authority_failure(
            codeyard_identity::sessions::AccountAuthorityError::PasswordChangeRequired,
        ));
    }
    Ok(user)
}

fn optional_user_allowed(user: &codeyard_db_core::models::User) -> bool {
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
        headers.insert(
            header::COOKIE,
            format!("session_id=1.{id}").parse().unwrap(),
        );
        assert_eq!(
            extract_session(&headers),
            Some((Prefix::new(1), id.clone()))
        );
        headers.insert(
            header::COOKIE,
            format!("session_id=01.{id}").parse().unwrap(),
        );
        assert!(extract_session(&headers).is_none());
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
        let cookie = build_session_cookie(&id, i64::MAX, Prefix::ZERO).unwrap();
        assert!(cookie.contains("Max-Age=31536000"));
    }

    #[test]
    fn optional_identity_rejects_banned_accounts_including_admins() {
        let mut user: codeyard_db_core::models::User = serde_json::from_value(serde_json::json!({
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
