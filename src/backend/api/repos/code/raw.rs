use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::{check_repo_read_access, readable_repository};
use crate::backend::api::repos::chrome::{load, resolve};
use crate::backend::app::AppState;
use crate::backend::repositories::store::{repo_store, BlobBytes, BlobRange, RepoError};
use axum::extract::{OriginalUri, Path, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::{collections::HashMap, sync::Arc};

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
pub use codeyard_webui::raw_content_type;

pub(crate) fn download_limit() -> (u64, String) {
    let bytes = crate::backend::git::MAX_BLOB_MATERIALIZE_BYTES as u64;
    (bytes, format!("{} MiB", bytes / (1024 * 1024)))
}

fn limit_message() -> String {
    format!(
        "Raw responses are limited to {}; request a byte range.",
        download_limit().1
    )
}

fn raw_store_error(error: RepoError) -> (StatusCode, Json<ApiError>) {
    if error.is_response_too_large() {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError::new("payload_too_large", &limit_message())),
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
    method: Method,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let head = method == Method::HEAD;
    let (range, partial) = (if head {
        Ok((
            BlobRange::Window {
                offset: 0,
                length: 0,
            },
            false,
        ))
    } else {
        crate::backend::api::http::range::requested(&headers, None)
    })
    .map_err(|_| {
        (
            StatusCode::RANGE_NOT_SATISFIABLE,
            Json(ApiError::new(
                "invalid_range",
                "Byte range is not satisfiable",
            )),
        )
    })?;
    let permit = if head {
        None
    } else {
        Some(
            crate::backend::api::http::middleware::wait_for_slot(&state.repository_downloads)
                .await
                .ok_or_else(|| {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(ApiError::new("downloads_busy", "Repository downloads busy")),
                    )
                })?,
        )
    };
    let blob = repo_store(&repository, state.cluster.as_ref())
        .blob_range(&git_ref, &path, range)
        .await
        .map_err(raw_store_error)?;
    // Access may have been lost while the blob was read.
    check_repo_read_access(&state.pool, &repository, &headers).await?;
    Ok(raw_response(&path, blob, partial, permit))
}

pub(crate) fn raw_response(
    path: &str,
    blob: BlobBytes,
    partial: bool,
    permit: impl Into<Option<tokio::sync::OwnedSemaphorePermit>>,
) -> Response {
    let permit = permit.into();
    let head = permit.is_none();
    let BlobBytes {
        id: _,
        bytes,
        is_binary,
        size,
        offset,
        ..
    } = blob;
    let length = if head { size } else { bytes.len() as u64 };
    let mut response = Response::new(match permit {
        Some(permit) => {
            crate::backend::api::http::middleware::buffered_download_body(bytes, permit)
        }
        None => axum::body::Body::empty(),
    });
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
            offset + length.saturating_sub(1),
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
    method: Method,
    OriginalUri(uri): OriginalUri,
    super::GitPathParams(params): super::GitPathParams,
) -> Response {
    if headers.contains_key(header::AUTHORIZATION) {
        return token_raw_page(state, headers, method, &params).await;
    }
    let head = method == Method::HEAD;
    let (range, partial) = match if head {
        Ok((
            BlobRange::Window {
                offset: 0,
                length: 0,
            },
            false,
        ))
    } else {
        crate::backend::api::http::range::requested(&headers, None)
    } {
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
    let permit = if head {
        None
    } else {
        let Some(permit) =
            crate::backend::api::http::middleware::wait_for_slot(&ctx.state.repository_downloads)
                .await
        else {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Repository downloads busy.",
            );
        };
        Some(permit)
    };
    let blob = repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .blob_range(&resolved.reference, &resolved.path, range)
        .await;
    match blob {
        Ok(blob) => {
            if let Err((status, _)) =
                check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers).await
            {
                return ctx.error(status, "Unable to download this repository file.");
            }
            ctx.with_cookie(raw_response(&resolved.path, blob, partial, permit))
        }
        Err(error) if error.is_response_too_large() => {
            ctx.error(StatusCode::PAYLOAD_TOO_LARGE, &limit_message())
        }
        Err(error) if error.is_range_not_satisfiable() => ctx.error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "Byte range is not satisfiable.",
        ),
        Err(error) => {
            crate::backend::api::repos::chrome::store_error(&ctx, Some(&loaded.chrome), error)
        }
    }
}

/// REST contents documents advertise this same native URL. Bearer clients retain their
/// repository-scoped token instead of being downgraded to anonymous browser visitors.
async fn token_raw_page(
    state: Arc<AppState>,
    headers: HeaderMap,
    method: Method,
    params: &HashMap<String, String>,
) -> Response {
    let (repository, authentication) = match crate::backend::api::github::repository(
        &state,
        &headers,
        &params["owner"],
        &params["repo"],
        false,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let Some(authentication) = authentication else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let credential = crate::backend::repositories::git_sessions::ReceiveCredential::Token(
        authentication.token.id,
    );
    if credential
        .authorize(&state.pool, &repository, false)
        .await
        .is_err()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let store = repo_store(&repository, state.cluster.as_ref());
    let refs = match store.refs().await {
        Ok(refs) => refs,
        Err(error) => return raw_store_error(error).into_response(),
    };
    let names = refs
        .keys()
        .flat_map(|name| {
            let short = name
                .strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/tags/"));
            std::iter::once(name.clone()).chain(short.map(str::to_owned))
        })
        .collect::<Vec<_>>();
    use codeyard_webui::pages::repos::ref_path::{resolve_repo_ref_path, split_path};
    let Some(resolved) = resolve_repo_ref_path(&split_path(&params["ref_path"]), &names, true)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let head = method == Method::HEAD;
    let (range, partial) = match if head {
        Ok((
            BlobRange::Window {
                offset: 0,
                length: 0,
            },
            false,
        ))
    } else {
        crate::backend::api::http::range::requested(&headers, None)
    } {
        Ok(range) => range,
        Err(()) => return StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
    };
    let permit = if head {
        None
    } else {
        let Some(permit) =
            crate::backend::api::http::middleware::wait_for_slot(&state.repository_downloads).await
        else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        Some(permit)
    };
    let blob = match store
        .blob_range(&resolved.reference, &resolved.path, range)
        .await
    {
        Ok(blob) => blob,
        Err(error) => return raw_store_error(error).into_response(),
    };
    let current =
        match crate::backend::repositories::access::current_repository(&state.pool, repository.id)
            .await
        {
            Ok(current) => current,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
    if credential
        .authorize(&state.pool, &current, false)
        .await
        .is_err()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    raw_response(&resolved.path, blob, partial, permit)
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
