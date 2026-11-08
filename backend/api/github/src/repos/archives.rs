//! Archive redirects pin the revision, and the download repeats the normal read authorization.
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::middleware::wait_for_slot;
use codeyard_api_http::shared::repos::code::archive::archive_response;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_git::ArchiveFormat;
use codeyard_git_storage::api_shared::github::contents::store_error;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use codeyard_types::urls::repo_url;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default, Deserialize)]
struct Download {
    #[serde(default)]
    download: bool,
}

async fn archive(
    state: Arc<AppState>,
    headers: HeaderMap,
    params: HashMap<String, String>,
    query: Download,
    format: ArchiveFormat,
) -> Result<Response> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, _) = repository(&state, &headers, owner, name, false).await?;
    let reference = params.get("ref").unwrap_or(&repo.default_branch);
    let store = repo_store(&repo, state.cluster.as_ref());
    let commit = store.commit(reference).await.map_err(store_error)?;
    let (segment, extension) = match format {
        ArchiveFormat::Zip => ("zipball", "zip"),
        ArchiveFormat::TarGz => ("tarball", "tar.gz"),
    };
    if !query.download {
        let location = format!(
            "{}/{segment}/{}?download=true",
            repo_url(owner, name),
            commit.sha
        );
        return Ok((
            StatusCode::FOUND,
            [
                (header::LOCATION, location),
                (header::CACHE_CONTROL, "no-store, private".into()),
            ],
        )
            .into_response());
    }
    let permit = wait_for_slot(&state.repository_downloads)
        .await
        .ok_or_else(|| Error::unavailable(()))?;
    let prefix = codeyard_api_http::shared::repos::code::archive::archive_prefix(
        &format!("{owner}-{name}"),
        codeyard_types::git_ids::short_sha(&commit.sha),
    );
    let bytes = store
        .archive(&commit.sha, &prefix, format)
        .await
        .map_err(store_error)?;
    crate::auth::recheck_repository_read(&state, &headers, repo.id, "contents").await?;
    Ok(archive_response(
        bytes,
        &format!("{prefix}.{extension}"),
        format,
        permit,
    ))
}

async fn zip(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<Download>,
) -> Result<Response> {
    archive(state, headers, params, query, ArchiveFormat::Zip).await
}
async fn tar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<Download>,
) -> Result<Response> {
    archive(state, headers, params, query, ArchiveFormat::TarGz).await
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/download-zipball-archive", get(zip)),
        ("repos/download-tarball-archive", get(tar)),
    ]
}
pub(crate) fn aliases() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/repos/:owner/:repo/zipball", get(zip))
        .cell_route("/repos/:owner/:repo/tarball", get(tar))
}
