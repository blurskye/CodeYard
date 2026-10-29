//! One single-range header contract for immutable HTTP downloads.
use crate::backend::storage::range::ByteRange;
use axum::http::{header, HeaderMap};

pub(crate) fn requested(headers: &HeaderMap) -> Result<(ByteRange, bool), ()> {
    requested_header(headers, header::RANGE.as_str())
}

pub(crate) fn requested_header(headers: &HeaderMap, name: &str) -> Result<(ByteRange, bool), ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else { return Ok((ByteRange::Full, false)); };
    if values.next().is_some() { return Err(()); }
    Ok((ByteRange::parse(value.to_str().map_err(|_| ())?)?, true))
}
