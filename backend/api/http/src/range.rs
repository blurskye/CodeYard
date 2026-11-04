//! One single-range header contract for immutable HTTP downloads.
use axum::http::header;
use axum::http::HeaderMap;
use codeyard_storage::range::ByteRange;

/// The requested `Range`, or `Full`. An `If-Range` that does not equal the response's strong
/// validator asks for the complete representation (RFC 9110 section 13.1.5). Without a
/// validator, such as a response with no ETag or Last-Modified, no `If-Range` can match.
#[allow(
    clippy::result_unit_err,
    reason = "An unusable range header has no error detail to report."
)]
pub fn requested(headers: &HeaderMap, validator: Option<&str>) -> Result<(ByteRange, bool), ()> {
    if let Some(condition) = headers.get(header::IF_RANGE) {
        if validator.is_none_or(|validator| condition.as_bytes() != validator.as_bytes()) {
            return Ok((ByteRange::Full, false));
        }
    }
    requested_header(headers, header::RANGE.as_str())
}

pub(crate) fn requested_header(headers: &HeaderMap, name: &str) -> Result<(ByteRange, bool), ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok((ByteRange::Full, false));
    };
    if values.next().is_some() {
        return Err(());
    }
    Ok((
        ByteRange::parse(value.to_str().map_err(|_| ())?).map_err(|_| ())?,
        true,
    ))
}
