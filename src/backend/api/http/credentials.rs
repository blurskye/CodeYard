//! Single bearer credentials used by the Actions runtime HTTP protocols.
use axum::http::{header, HeaderMap};

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() { return None; }
    let (scheme, token) = value.split_once(' ')?;
    let base = token.trim_end_matches('=');
    (scheme.eq_ignore_ascii_case("bearer") && token.len() <= 4096 && !base.is_empty()
        && base.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte)))
        .then_some(token)
}
