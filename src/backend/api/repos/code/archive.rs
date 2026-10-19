use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{load_refs_from_repo, repository_default_branch};
use crate::backend::api::repos::responses::repo_store_error;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::git::ArchiveFormat;
use crate::backend::repositories::store::{repo_store, ErrorKind, RepoError};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct ArchiveQuery {
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
}

pub async fn download_archive(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<ArchiveQuery>,
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

    let git_ref = match query.git_ref {
        Some(git_ref) => git_ref,
        None => repository_default_branch(&state, &repository).await?,
    };
    let archive_prefix = format!("{}-{}", repo, git_ref.replace('/', "-"));
    let filename = format!("{}.zip", archive_prefix);

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
    let archive_bytes = repo_store(&repository, state.cluster.as_ref())
        .archive(&git_ref, &archive_prefix, ArchiveFormat::Zip)
        .await
        .map_err(archive_error)?;

    Ok(archive_response(
        archive_bytes,
        &filename,
        ArchiveFormat::Zip,
        permit,
    ))
}

/// Refs that Git cannot resolve stay a 400 `invalid_ref`. Unreachable storage is a service
/// failure and takes the shared 503 mapping.
fn archive_error(error: RepoError) -> (StatusCode, Json<ApiError>) {
    if error.is_response_too_large() {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError::new(
                "payload_too_large",
                "Repository archives are limited to 32 MiB",
            )),
        )
    } else if error.kind == ErrorKind::Unavailable {
        repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE)
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "invalid_ref",
                "Repository operation is unavailable",
            )),
        )
    }
}

fn archive_response(
    archive_bytes: Vec<u8>,
    filename: &str,
    format: ArchiveFormat,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> axum::response::Response {
    let length = archive_bytes.len();
    let mut response = axum::response::Response::new(
        crate::backend::api::http::middleware::buffered_download_body(archive_bytes, permit),
    );
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

/// `/:owner/:repo/archive/{ref}.zip` or `{ref}.tar.gz`: the tree at a branch, tag or commit.
/// The ref may contain slashes, so the extension is the last part of the path.
pub(in crate::backend::api::repos) async fn page(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path((owner, name, archive)): axum::extract::Path<(String, String, String)>,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> axum::response::Response {
    let ctx =
        match crate::backend::api::http::context::PageContext::load(state, headers, &uri).await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    let Some((reference, format)) = parse_archive_name(&archive) else {
        return ctx.error(StatusCode::NOT_FOUND, "Archive format not found.");
    };
    let repository =
        match db::repositories::records::get_repository(&ctx.state.pool, &owner, &name).await {
            Ok(Some(repository)) => repository,
            Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Repository not found."),
            Err(_) => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Repository storage is unavailable.",
                )
            }
        };
    if let Err((status, _)) =
        check_repo_read_access(&ctx.state.pool, &repository, &ctx.headers).await
    {
        return ctx.error(status, "Unable to download this repository archive.");
    }
    // A tag arrives as `refs/tags/NAME`; the folder and file keep the short name.
    let short = reference.strip_prefix("refs/tags/").unwrap_or(reference);
    let prefix = format!("{}-{}", name, short.replace('/', "-"));
    let Ok(permit) = ctx.state.repository_downloads.clone().try_acquire_owned() else {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Repository downloads busy.",
        );
    };
    let store = repo_store(&repository, ctx.state.cluster.as_ref());
    match store.archive(reference, &prefix, format).await {
        Ok(bytes) => {
            let extension = match format {
                ArchiveFormat::Zip => "zip",
                ArchiveFormat::TarGz => "tar.gz",
            };
            ctx.with_cookie(crate::backend::api::http::render::secure_html_headers(
                archive_response(bytes, &format!("{prefix}.{extension}"), format, permit),
            ))
        }
        Err(error) if error.is_response_too_large() => ctx.error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Repository archives are limited to 32 MiB.",
        ),
        Err(error) if error.kind == ErrorKind::Invalid => ctx.error(
            StatusCode::NOT_FOUND,
            "No branch, tag or commit with this name.",
        ),
        Err(error) => crate::backend::api::repos::chrome::store_error(&ctx, None, error),
    }
}

/// The ref and format named by `{ref}.zip` or `{ref}.tar.gz`. An empty ref has no archive.
fn parse_archive_name(name: &str) -> Option<(&str, ArchiveFormat)> {
    let (reference, format) = match name.strip_suffix(".tar.gz") {
        Some(reference) => (reference, ArchiveFormat::TarGz),
        None => (name.strip_suffix(".zip")?, ArchiveFormat::Zip),
    };
    (!reference.is_empty()).then_some((reference, format))
}

#[cfg(test)]
mod archive_error_tests {
    use super::*;

    #[test]
    fn archive_failures_map_by_kind() {
        assert_eq!(
            archive_error(RepoError::response_too_large()).0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            archive_error(RepoError::authority_unavailable()).0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            archive_error(RepoError::storage_node_unavailable()).0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            archive_error(RepoError::new(ErrorKind::Invalid, "bad ref")).0,
            StatusCode::BAD_REQUEST
        );
    }
}
