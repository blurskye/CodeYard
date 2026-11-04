use crate::access::readable_repository;
use crate::chrome::repository_default_branch;
use crate::responses::repo_store_error;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::shared::repos::access::check_repo_read_access;
use codeyard_app::app::AppState;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::ArchiveFormat;
use codeyard_repos_core::repositories::store::repo_store;
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
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let git_ref = match query.git_ref {
        Some(git_ref) => git_ref,
        None => repository_default_branch(&state, &repository).await?,
    };
    let archive_prefix = archive_prefix(&repo, &git_ref);
    let filename = format!("{}.zip", archive_prefix);

    let permit = codeyard_api_http::middleware::wait_for_slot(&state.repository_downloads)
        .await
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new("downloads_busy", "Repository downloads busy")),
            )
        })?;
    let archive_bytes = repo_store(&repository, state.cluster.as_ref())
        .archive(&git_ref, &archive_prefix, ArchiveFormat::Zip)
        .await
        .map_err(archive_error)?;
    check_repo_read_access(&state.pool, &repository, &headers).await?;

    Ok(archive_response(
        archive_bytes,
        &filename,
        ArchiveFormat::Zip,
        permit,
    ))
}

/// An unknown ref is a 404 and an unreadable one a 400. Unreachable storage is a 503.
fn archive_error(error: RepoError) -> (StatusCode, Json<ApiError>) {
    if error.is_response_too_large() {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError::new(
                "payload_too_large",
                "Repository archives are limited to 32 MiB",
            )),
        )
    } else {
        repo_store_error(error, StatusCode::BAD_REQUEST)
    }
}

/// `/:owner/:repo/archive/{ref}.zip` or `{ref}.tar.gz`: the tree at a branch, tag or commit.
/// The ref may contain slashes, so the extension is the last part of the path.
pub(crate) async fn page(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path((owner, name, archive)): axum::extract::Path<(String, String, String)>,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> axum::response::Response {
    let ctx = match codeyard_api_http::context::PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let Some((reference, format)) = parse_archive_name(&archive) else {
        return ctx.error(StatusCode::NOT_FOUND, "Archive format not found.");
    };
    let repository = match codeyard_db_repos::repositories::records::get_repository(
        &ctx.state.pool,
        &owner,
        &name,
    )
    .await
    {
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
    let prefix = archive_prefix(&name, reference);
    let Some(permit) =
        codeyard_api_http::middleware::wait_for_slot(&ctx.state.repository_downloads).await
    else {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Repository downloads busy.",
        );
    };
    let store = repo_store(&repository, ctx.state.cluster.as_ref());
    match store.archive(reference, &prefix, format).await {
        Ok(bytes) => {
            if let Err((status, _)) =
                check_repo_read_access(&ctx.state.pool, &repository, &ctx.headers).await
            {
                return ctx.error(status, "Unable to download this repository archive.");
            }
            let extension = match format {
                ArchiveFormat::Zip => "zip",
                ArchiveFormat::TarGz => "tar.gz",
            };
            ctx.with_cookie(codeyard_api_http::render::secure_html_headers(
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
        Err(error) => codeyard_api_http::shared::repos::chrome::store_error(&ctx, None, error),
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

use codeyard_api_http::shared::repos::code::archive::archive_prefix;

use codeyard_api_http::shared::repos::code::archive::archive_response;

#[cfg(test)]
mod archive_error_tests {
    use super::*;

    #[test]
    fn archive_folders_are_bounded_and_disambiguated() {
        let reference = vec!["x".repeat(100); 3].join("/");
        let prefix = archive_prefix("demo", &reference);
        assert!(prefix.len() <= 200);
        assert!(prefix
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)));
        assert_ne!(prefix, archive_prefix("demo", &format!("{reference}y")));
        assert_eq!(archive_prefix("demo", "refs/tags/v1"), "demo-v1");
        assert!(archive_prefix("demo", "é\"test").is_ascii());
    }

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
        assert_eq!(
            archive_error(RepoError::new(ErrorKind::NotFound, "missing ref")).0,
            StatusCode::NOT_FOUND
        );
    }
}
