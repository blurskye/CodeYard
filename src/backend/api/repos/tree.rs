use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{
    commit, default_ref, load_refs_from_repo, ref_names, resolve, store_error, Loaded,
};
use crate::backend::api::repos::references::resolve_references;
use crate::backend::api::repos::responses::CommitResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::{repo_store, TreeEntry};
use crate::webui::pages::repositories as view;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct TreeEntryResponse {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: Option<u64>,
    pub last_commit: Option<CommitResponse>,
}

impl From<TreeEntry> for TreeEntryResponse {
    fn from(entry: TreeEntry) -> Self {
        let last_commit = entry
            .last_commit
            .map(|commit| CommitResponse::from_commit(commit, None));

        Self {
            name: entry.name,
            path: entry.path,
            entry_type: entry.entry_type,
            size: entry.size,
            last_commit,
        }
    }
}

pub async fn get_tree(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, git_ref, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<TreeEntryResponse>>, (StatusCode, Json<ApiError>)> {
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
        return Ok(Json(Vec::new()));
    }

    let tree = repo_store(&repository, state.cluster.as_ref())
        .tree(&git_ref, &path)
        .await
        .map_err(|e| {
            crate::backend::api::repos::responses::repo_store_error(
                e,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(Json(
        tree.into_iter().map(TreeEntryResponse::from).collect(),
    ))
}

pub async fn get_tree_root(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, git_ref)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<TreeEntryResponse>>, (StatusCode, Json<ApiError>)> {
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
        return Ok(Json(Vec::new()));
    }

    let tree = repo_store(&repository, state.cluster.as_ref())
        .tree(&git_ref, "")
        .await
        .map_err(|e| {
            crate::backend::api::repos::responses::repo_store_error(
                e,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(Json(
        tree.into_iter().map(TreeEntryResponse::from).collect(),
    ))
}

/// How many refs under `prefix` the repository has, from the refs the page already read.
fn count_refs(loaded: &Loaded, prefix: &str) -> usize {
    loaded
        .refs
        .keys()
        .filter(|name| name.starts_with(prefix))
        .count()
}

async fn tree_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    let resolved = if tail.is_empty() {
        view::ref_path::ResolvedRefPath {
            reference: default_ref(loaded),
            path: String::new(),
        }
    } else {
        resolve(ctx, loaded, tail, false)?
    };
    let mut page = view::TreePage {
        reference: resolved.reference.clone(),
        path: resolved.path.clone(),
        refs: ref_names(loaded),
        branch_count: count_refs(loaded, "refs/heads/"),
        tag_count: count_refs(loaded, "refs/tags/"),
        entries: Vec::new(),
        readme: None,
        latest_commit: None,
        references: Default::default(),
        empty: loaded.refs.is_empty(),
        http_clone: format!(
            "{}/{}/{}.git",
            crate::backend::api::config::http_clone_base().trim_end_matches('/'),
            loaded.chrome.owner,
            loaded.chrome.name
        ),
        ssh_clone: {
            let base = crate::backend::api::config::ssh_clone_base();
            if base.is_empty() {
                String::new()
            } else {
                format!(
                    "{}/{}/{}.git",
                    base.trim_end_matches('/'),
                    loaded.chrome.owner,
                    loaded.chrome.name
                )
            }
        },
    };
    if !page.empty {
        let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
        let reference = resolved.reference.clone();
        let fail = |error| store_error(ctx, Some(&loaded.chrome), error);
        let last = store.commit(&reference).await.map_err(fail)?;
        // The size check and blob read must refer to the same commit, even during a push.
        let entries = store.tree(&last.sha, &resolved.path).await.map_err(fail)?;
        // A folder's header shows the newest commit that touched it. Each row already
        // carries the newest commit that touched that entry, so the newest row is the
        // folder's. The root has no path to scope to and keeps the branch head.
        let folder_commit = if resolved.path.is_empty() {
            Some(last.clone())
        } else {
            entries
                .iter()
                .filter_map(|entry| entry.last_commit.as_ref())
                .max_by_key(|info| info.committer_timestamp)
                .cloned()
        };
        let readme_entry = entries.iter().find(|entry| {
            entry.entry_type == "file" && entry.name.eq_ignore_ascii_case("readme.md")
        });
        let readme = match readme_entry {
            Some(entry)
                if entry
                    .size
                    .is_some_and(|size| size <= view::MAX_README_BYTES) =>
            {
                Some(store.blob(&last.sha, &entry.path).await.map_err(fail)?)
            }
            _ => None,
        };
        page.entries = entries
            .into_iter()
            .map(|entry| view::TreeEntry {
                name: entry.name,
                path: entry.path,
                kind: if entry.entry_type == "dir" {
                    "tree".into()
                } else {
                    entry.entry_type
                },
                size: entry.size,
                last_commit: entry.last_commit.map(commit),
            })
            .collect();
        page.latest_commit = folder_commit.map(commit);
        page.readme = readme
            .filter(|blob| !blob.is_binary)
            .map(|blob| blob.content);
        if let Some(readme) = &page.readme {
            page.references = Box::new(
                resolve_references(
                    ctx,
                    &loaded.repository,
                    &loaded.chrome.owner,
                    &loaded.chrome.name,
                    &[readme.as_str()],
                    // The README renders as a document.
                    true,
                )
                .await?,
            );
        }
    } else if !page.path.is_empty() {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository is empty."));
    }
    Ok(ctx.page_model(
        &format!("{}/{}", loaded.chrome.owner, loaded.chrome.name),
        crate::webui::app::Page::Repositories(view::Page::Tree {
            repo: loaded.chrome.clone(),
            page,
        }),
    ))
}

pub(super) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx = match crate::backend::api::context::PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded =
        match crate::backend::api::repos::chrome::load(&ctx, &params["owner"], &params["repo"])
            .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    match tree_page(&ctx, &loaded, tail).await {
        Ok(response) | Err(response) => response,
    }
}
