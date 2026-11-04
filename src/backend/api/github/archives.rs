//! Archive redirects pin the revision, and the download repeats the normal read authorization.
use crate::backend::api::github::contents::store_error;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::api::http::middleware::wait_for_slot;
use crate::backend::api::repos::code::archive::archive_response;
use crate::backend::app::AppState;
use crate::backend::repositories::store::repo_store;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Router,
};
use codeyard_git::ArchiveFormat;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

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
    let prefix = crate::backend::api::repos::code::archive::archive_prefix(
        &format!("{owner}-{name}"),
        codeyard_webui::components::short_sha(&commit.sha),
    );
    let bytes = store
        .archive(&commit.sha, &prefix, format)
        .await
        .map_err(store_error)?;
    crate::backend::api::github::auth::recheck_repository_read(
        &state, &headers, repo.id, "contents",
    )
    .await?;
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
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/download-zipball-archive", get(zip)),
        ("repos/download-tarball-archive", get(tar)),
    ]
}
pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new()
        .route("/repos/:owner/:repo/zipball", get(zip))
        .route("/repos/:owner/:repo/tarball", get(tar))
}
