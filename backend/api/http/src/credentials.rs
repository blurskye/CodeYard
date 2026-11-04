//! Bounded Authorization header decoding; each adapter owns the meaning of the credential.
use axum::http::header;
use axum::http::HeaderMap;
use base64::engine::general_purpose::STANDARD;

pub fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") || encoded.len() > 2048 {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_owned(), password.to_owned()))
}

/// One bearer credential, as the Actions runtime HTTP protocols send it.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.split_once(' ')?;
    let base = token.trim_end_matches('=');
    (scheme.eq_ignore_ascii_case("bearer")
        && token.len() <= 4096
        && !base.is_empty()
        && base
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte)))
    .then_some(token)
}
