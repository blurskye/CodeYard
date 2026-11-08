use axum::http::header;
use codeyard_git::ArchiveFormat;
pub fn archive_response(
    archive_bytes: Vec<u8>,
    filename: &str,
    format: ArchiveFormat,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> axum::response::Response {
    let length = archive_bytes.len();
    let mut response = axum::response::Response::new(crate::middleware::buffered_download_body(
        archive_bytes,
        permit,
    ));
    let response_headers = response.headers_mut();
    response_headers.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
    response_headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(match format {
            ArchiveFormat::Zip => "application/zip",
            ArchiveFormat::TarGz => "application/gzip",
        }),
    );
    response_headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store, private"),
    );
    response_headers.insert(
        header::VARY,
        axum::http::HeaderValue::from_static("Cookie, Authorization"),
    );
    response_headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    response_headers.insert(
        header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{}\"", filename))
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("attachment")),
    );

    response
}

/// Leave room for the extension below common filesystems' 255-byte component limit.
pub fn archive_prefix(repo: &str, reference: &str) -> String {
    use sha2::{Digest, Sha256};
    let short = reference.strip_prefix("refs/tags/").unwrap_or(reference);
    let original = format!("{repo}-{}", short.replace('/', "-"));
    let mut prefix: String = original
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if prefix.len() > 200 || prefix != original {
        prefix.truncate(180);
        prefix.push_str(
            &format!(
                "-{}",
                hex::encode(Sha256::digest(format!("{repo}-{short}").as_bytes()))
            )[..13],
        );
    }
    prefix
}
