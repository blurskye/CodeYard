//! Bounded HTTP Basic decoding; each adapter owns the meaning of the two fields.
use axum::http::{header, HeaderMap};
use base64::{engine::general_purpose::STANDARD, Engine};

pub(crate) fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() { return None; }
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") || encoded.len() > 2048 { return None; }
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_owned(), password.to_owned()))
}
