use crate::repos::commits::diff_files;
use crate::repos::commits::documents;
use crate::repos::commits::read_document;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_core::models::Repository;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_repos_core::repositories::reads::same_fork_network;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_webui::pages::repos::ref_path::encode_path as path_url;
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
    let source = if source_owner == owner {
        base.clone()
    } else {
        let mut candidates = codeyard_db_repos::repositories::discovery::comparison_forks(
            &state.pool,
            base.id,
            source_owner,
        )
        .await
        .map_err(Error::unavailable)?;
        if candidates.is_empty() || (candidates.len() > 1 && candidates[0].name != base.name) {
            return Err(Error::missing());
        }
        let source = candidates.remove(0);
        crate::auth::recheck_repository_read(state, headers, source.id, "contents").await?;
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
        .comparison_from(
            &head_store,
            &base,
            &head,
            offset,
            if paginated { limit } else { 0 },
        )
        .await
        .map_err(store_error)?;
    if !paginated {
        // Resolve and count first; only the final window spends the commit metadata budget.
        // Pin the resolved identities across the second read.
        let (base_sha, head_sha) = (
            comparison.base_commit.sha.clone(),
            comparison.head_sha.clone(),
        );
        comparison = base_store
            .comparison_from(
                &head_store,
                &base_sha,
                &head_sha,
                comparison.ahead_by.saturating_sub(limit),
                limit,
            )
            .await
            .map_err(store_error)?;
    }
    if let Some(media) = crate::diff_media::requested(&headers) {
        if media == crate::diff_media::Media::Patch {
            return crate::diff_media::series(
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
        return Ok(crate::diff_media::response(
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
        if source == owner.as_str() {
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
        codeyard_model::http_config::link_origin()
    );
    let html_url = format!("{site}/{}", path_url(&spec));
    let mut value = json!({
        "url": format!("{}/compare/{}", repo_url(&owner, &name), codeyard_webui::encode_component(&spec)),
        "html_url": html_url, "permalink_url": format!("{site}/{}", path_url(&permalink)),
        "diff_url": format!("{html_url}.diff"), "patch_url": format!("{html_url}.patch"),
        "base_commit": read_document(&state, base_repo.id, &comparison.base_commit, &base_owner, &base_repo.name).await?,
        "merge_base_commit": read_document(&state, base_repo.id, &comparison.merge_base_commit, &base_owner, &base_repo.name).await?,
        "ahead_by": comparison.ahead_by, "behind_by": comparison.behind_by, "total_commits": comparison.ahead_by, "status": status,
        "commits": documents(&state, head_repo.id, &comparison.commits, &head_owner, &head_repo.name).await?
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
            &head_repo.name,
            &comparison.head_sha,
            Some((
                &base_owner,
                &base_repo.name,
                &comparison.merge_base_commit.sha
            ))
        ));
    }
    let response_headers = if paginated {
        page.headers(&uri, comparison.ahead_by > offset.saturating_add(limit))?
    } else {
        HeaderMap::new()
    };
    Ok((response_headers, Json(value)).into_response())
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/compare-commits", get(compare))]
}
