use super::{
    auth,
    commits::{diff_files, documents, read_document},
    contents::{path_url, store_error},
    pagination::Pagination,
    repo_url, repository, Error, Result,
};
use crate::backend::{
    app::AppState,
    db::models::Repository,
    repositories::{reads::same_fork_network, store::repo_store},
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde_json::json;
use std::sync::Arc;

pub(super) async fn side(
    state: &AppState,
    headers: &HeaderMap,
    base: &Repository,
    owner: &str,
    reference: &str,
) -> Result<(Repository, String, String)> {
    let (source_owner, revision) = reference.split_once(':').unwrap_or((owner, reference));
    if source_owner.is_empty() || revision.is_empty() {
        return Err(Error::missing());
    }
    let source = if source_owner.eq_ignore_ascii_case(owner) {
        base.clone()
    } else {
        let (source, source_auth) =
            repository(state, headers, source_owner, &base.name, false).await?;
        // Job tokens and App credentials are bound to their repositories, so a readable fork
        // still needs a grant.
        if let Some(source_auth) = source_auth {
            source_auth
                .repository_scope(&state.pool, source.id, "contents", false)
                .await
                .map_err(auth::scope_error)?;
        }
        source
    };
    if !same_fork_network(&state.pool, base, &source)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok((source, source_owner.to_owned(), revision.to_owned()))
}

async fn compare(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, spec)): Path<(String, String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (base, head) = spec.split_once("...").ok_or_else(Error::missing)?;
    let (base_repo, base_owner, base) = side(&state, &headers, &repo, &owner, base).await?;
    let (head_repo, head_owner, head) = side(&state, &headers, &repo, &owner, head).await?;
    let paginated = page.page.is_some() || page.per_page.is_some();
    let (offset, limit) = if paginated { page.window()? } else { (0, 250) };
    let base_store = repo_store(&base_repo, state.cluster.as_ref());
    let head_store = repo_store(&head_repo, state.cluster.as_ref());
    let mut comparison = base_store
        .comparison_from(&head_store, &base, &head, offset, limit)
        .await
        .map_err(store_error)?;
    if !paginated && comparison.ahead_by > limit {
        // An unpaginated list ends with the newest commit, so it takes the last window instead.
        // The first answer's commits are reused, so a branch that moves meanwhile changes nothing.
        let (base_sha, head_sha) = (
            comparison.base_commit.sha.clone(),
            comparison.head_sha.clone(),
        );
        comparison = base_store
            .comparison_from(
                &head_store,
                &base_sha,
                &head_sha,
                comparison.ahead_by - limit,
                limit,
            )
            .await
            .map_err(store_error)?;
    }
    if let Some(media) = super::diff_media::requested(&headers) {
        if media == super::diff_media::Media::Patch {
            return super::diff_media::series(
                &head_store,
                &comparison.commits,
                comparison.ahead_by,
            )
            .await
            .map_err(store_error);
        }
        let diff = base_store
            .diff_from(
                &head_store,
                &comparison.base_commit.sha,
                &comparison.head_sha,
            )
            .await
            .map_err(store_error)?;
        return Ok(super::diff_media::response(
            media,
            &comparison.base_commit,
            &diff.files,
        ));
    }
    let status = match (comparison.ahead_by > 0, comparison.behind_by > 0) {
        (false, false) => "identical",
        (true, false) => "ahead",
        (false, true) => "behind",
        (true, true) => "diverged",
    };
    let qualify = |source: &str, reference: &str| {
        if source.eq_ignore_ascii_case(&owner) {
            reference.to_owned()
        } else {
            format!("{source}:{reference}")
        }
    };
    let permalink = format!(
        "{}...{}",
        qualify(&base_owner, &comparison.base_commit.sha),
        qualify(&head_owner, &comparison.head_sha)
    );
    let site = format!(
        "{}/{owner}/{name}/compare",
        crate::backend::api::http::config::link_origin()
    );
    let html_url = format!("{site}/{}", path_url(&spec));
    let mut value = json!({
        "url": format!("{}/compare/{}", repo_url(&owner, &name), crate::webui::encode_component(&spec)),
        "html_url": html_url, "permalink_url": format!("{site}/{}", path_url(&permalink)),
        "diff_url": format!("{html_url}.diff"), "patch_url": format!("{html_url}.patch"),
        "base_commit": read_document(&state, base_repo.id, &comparison.base_commit, &base_owner, &name).await?,
        "merge_base_commit": read_document(&state, base_repo.id, &comparison.merge_base_commit, &base_owner, &name).await?,
        "ahead_by": comparison.ahead_by, "behind_by": comparison.behind_by, "total_commits": comparison.ahead_by, "status": status,
        "commits": documents(&state, head_repo.id, &comparison.commits, &head_owner, &name).await?
    });
    if page.page.unwrap_or(1) == 1 {
        let mut diff = base_store
            .diff_from(
                &head_store,
                &comparison.base_commit.sha,
                &comparison.head_sha,
            )
            .await
            .map_err(store_error)?;
        diff.files.truncate(300);
        value["files"] = json!(diff_files(
            diff.files,
            &head_owner,
            &name,
            &comparison.head_sha
        ));
    }
    let response_headers = if paginated {
        page.headers(&uri, comparison.ahead_by > offset.saturating_add(limit))?
    } else {
        HeaderMap::new()
    };
    Ok((response_headers, Json(value)).into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/compare-commits", get(compare))]
}
