//! Reading multipart form parts with a size limit, so a text field is never buffered whole.
//! Reading multipart form parts with a size limit, so a text field is never buffered whole.
use axum::extract::multipart::Field;
use axum::extract::multipart::MultipartError;

/// Reads a text part chunk by chunk. `None` means the part is longer than `limit` bytes or is not
/// UTF-8; the caller decides how to refuse it.
pub async fn bounded_text(
    field: &mut Field<'_>,
    limit: usize,
) -> Result<Option<String>, MultipartError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8(bytes).ok())
}
