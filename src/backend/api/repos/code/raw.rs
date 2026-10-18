use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{load, load_refs_from_repo, resolve};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::{repo_store, BlobBytes, BlobRange, RepoError};
use axum::extract::{OriginalUri, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;

/// Content type for a raw blob.
///
/// The response also carries `X-Content-Type-Options: nosniff`, so the declared
/// type has to be *accurate*: browsers are told not to sniff, which makes a
/// generic `application/octet-stream` a real risk for media that pages embed
/// (an `<img>`/`<video>`/`<audio>` pointing at a repo file). Raster images,
/// audio, video, PDFs and fonts are therefore typed by extension; everything
/// else keeps the previous binary/text heuristic.
///
/// `image/svg+xml` is deliberately NOT used for `.svg`: the raw route serves
/// user content from this origin, and an SVG opened directly (not through
/// `<img>`) can run script in that origin. Raw SVG stays a non-executable type,
/// which is what the previous behaviour was too.
pub fn raw_content_type(path: &str, is_binary: bool) -> &'static str {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ if is_binary => "application/octet-stream",
        _ => "text/plain; charset=utf-8",
    }
}

fn requested_range(headers: &HeaderMap) -> Result<(BlobRange, bool), ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Ok((BlobRange::Full, false));
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    let range = match (start.is_empty(), end.is_empty()) {
        (false, false) => {
            let start = start.parse::<u64>().map_err(|_| ())?;
            let end = end.parse::<u64>().map_err(|_| ())?;
            if end < start {
                return Err(());
            }
            BlobRange::Bytes {
                start,
                end: Some(end),
            }
        }
        (false, true) => BlobRange::Bytes {
            start: start.parse::<u64>().map_err(|_| ())?,
            end: None,
        },
        (true, false) => {
            let length = end.parse::<u64>().map_err(|_| ())?;
            if length == 0 {
                return Err(());
            }
            BlobRange::Suffix { length }
        }
        (true, true) => return Err(()),
    };
    Ok((range, true))
}

fn raw_store_error(error: RepoError) -> (StatusCode, Json<ApiError>) {
    if error.is_response_too_large() {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError::new(
                "payload_too_large",
                "Raw responses are limited to 32 MiB; request a byte range",
            )),
        );
    }
    if error.is_range_not_satisfiable() {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            Json(ApiError::new(
                "invalid_range",
                "Byte range is not satisfiable",
            )),
        );
    }
    crate::backend::api::repos::responses::repo_store_error(
        error,
        StatusCode::INTERNAL_SERVER_ERROR,
    )
}

pub async fn get_raw_blob(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, git_ref, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Repository operation is unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_read_access(&state.pool, &repository, &headers).await?;

    let refs = load_refs_from_repo(&state, &repository, &format!("{}/{}", owner, repo)).await?;
    if refs.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Repository is empty")),
        ));
    }

    let (range, partial) = requested_range(&headers).map_err(|_| {
        (
            StatusCode::RANGE_NOT_SATISFIABLE,
            Json(ApiError::new(
                "invalid_range",
                "Byte range is not satisfiable",
            )),
        )
    })?;
    let permit = state
        .repository_downloads
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new("downloads_busy", "Repository downloads busy")),
            )
        })?;
    let blob = repo_store(&repository, state.cluster.as_ref())
        .blob_range(&git_ref, &path, range)
        .await
        .map_err(raw_store_error)?;

    Ok(raw_response(&path, blob, partial, permit))
}

pub(crate) fn raw_response(
    path: &str,
    blob: BlobBytes,
    partial: bool,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let BlobBytes {
        id: _,
        bytes,
        is_binary,
        size,
        offset,
    } = blob;
    let length = bytes.len();
    let mut response =
        Response::new(crate::backend::api::http::middleware::buffered_download_body(bytes, permit));
    *response.status_mut() = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let content_type = raw_content_type(path, is_binary);
    let content_range = partial.then(|| {
        format!(
            "bytes {}-{}/{}",
            offset,
            offset + length.saturating_sub(1) as u64,
            size
        )
    });
    let content_length = length.to_string();
    let content_range = content_range
        .as_deref()
        .and_then(|value| axum::http::HeaderValue::from_str(value).ok());
    let content_length = axum::http::HeaderValue::from_str(&content_length).unwrap();
    let response_headers = response.headers_mut();
    response_headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(content_type),
    );
    response_headers.insert(
        header::ACCEPT_RANGES,
        axum::http::HeaderValue::from_static("bytes"),
    );
    response_headers.insert(header::CONTENT_LENGTH, content_length);
    if let Some(content_range) = content_range {
        response_headers.insert(header::CONTENT_RANGE, content_range);
    }
    response_headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store, private"),
    );
    response_headers.insert(
        header::VARY,
        axum::http::HeaderValue::from_static("Cookie, Authorization"),
    );
    response_headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );

    response
}

/// Native raw URL: authorize the page reference, then return the original bytes.
pub async fn raw_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let (range, partial) = match requested_range(&headers) {
        Ok(range) => range,
        Err(()) => return StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
    };
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let owner = &params["owner"];
    let name = &params["repo"];
    let loaded = match load(&ctx, owner, name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let resolved = match resolve(&ctx, &loaded, &params["ref_path"], true) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Ok(permit) = ctx.state.repository_downloads.clone().try_acquire_owned() else {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Repository downloads busy.",
        );
    };
    let blob = repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .blob_range(&resolved.reference, &resolved.path, range)
        .await;
    match blob {
        Ok(blob) => ctx.with_cookie(raw_response(&resolved.path, blob, partial, permit)),
        Err(error) if error.is_response_too_large() => ctx.error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Raw responses are limited to 32 MiB; request a byte range.",
        ),
        Err(error) if error.is_range_not_satisfiable() => ctx.error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "Byte range is not satisfiable.",
        ),
        Err(error) => {
            crate::backend::api::repos::chrome::store_error(&ctx, Some(&loaded.chrome), error)
        }
    }
}

#[cfg(test)]
mod raw_content_type_tests {
    use super::raw_content_type;

    #[test]
    fn media_types_come_from_the_extension() {
        assert_eq!(raw_content_type("docs/pixel.png", true), "image/png");
        assert_eq!(raw_content_type("a/b/photo.JPEG", true), "image/jpeg");
        assert_eq!(raw_content_type("clip.mp4", true), "video/mp4");
        assert_eq!(raw_content_type("sound.mp3", true), "audio/mpeg");
        assert_eq!(raw_content_type("doc.pdf", true), "application/pdf");
        assert_eq!(raw_content_type("font.woff2", true), "font/woff2");
    }

    #[test]
    fn svg_is_not_typed_as_an_image() {
        // Documented security choice: raw user SVG must not become an
        // executable type on this origin.
        assert_eq!(
            raw_content_type("logo.svg", false),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            raw_content_type("logo.svg", true),
            "application/octet-stream"
        );
    }

    #[test]
    fn unknown_extensions_keep_the_binary_text_heuristic() {
        assert_eq!(
            raw_content_type("README.md", false),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            raw_content_type("archive.bin", true),
            "application/octet-stream"
        );
        assert_eq!(
            raw_content_type("Makefile", false),
            "text/plain; charset=utf-8"
        );
    }
}
