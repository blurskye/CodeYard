use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::readable_repository;
use crate::backend::api::repos::chrome::{
    branch_name, commit, default_ref, load_refs_from_repo, ref_names_under, resolve,
    root_if_missing, store_error, Loaded,
};
use crate::backend::api::repos::refs::references::resolve_references;
use crate::backend::api::repos::responses::CommitResponse;
use crate::backend::app::AppState;
use crate::backend::repositories::languages as repo_languages;
use crate::backend::repositories::releases;
use crate::backend::repositories::store::{repo_store, RepoStore, TreeEntry};
use crate::webui::components::languages::LanguageShare;
use crate::webui::pages::repos as view;
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

/// The entries of one folder as the repository pages show them.
pub(super) fn view_entries(entries: Vec<TreeEntry>) -> Vec<view::tree::TreeEntry> {
    entries
        .into_iter()
        .map(|entry| view::tree::TreeEntry {
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
        .collect()
}

pub async fn get_tree(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, git_ref, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<TreeEntryResponse>>, (StatusCode, Json<ApiError>)> {
    tree_entries(&state, &headers, (&owner, &repo), &git_ref, &path).await
}

pub async fn get_tree_root(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, git_ref)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<TreeEntryResponse>>, (StatusCode, Json<ApiError>)> {
    tree_entries(&state, &headers, (&owner, &repo), &git_ref, "").await
}

/// The entries of the folder at `path`, or none for a repository without refs.
async fn tree_entries(
    state: &AppState,
    headers: &HeaderMap,
    (owner, repo): (&str, &str),
    git_ref: &str,
    path: &str,
) -> Result<Json<Vec<TreeEntryResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, owner, repo, headers).await?;

    let refs = load_refs_from_repo(state, &repository).await?;
    if refs.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let tree = repo_store(&repository, state.cluster.as_ref())
        .tree(git_ref, path)
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

async fn tree_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    let resolved = if tail.is_empty() {
        view::ref_path::ResolvedRefPath {
            reference: default_ref(loaded),
            path: String::new(),
        }
    } else {
        resolve(ctx, loaded, tail, false)?
    };
    let mut page = view::tree::TreePage {
        reference: resolved.reference.clone(),
        path: resolved.path.clone(),
        branches: ref_names_under(loaded, "refs/heads/"),
        tags: ref_names_under(loaded, "refs/tags/"),
        entries: Vec::new(),
        folders: Vec::new(),
        languages: Vec::new(),
        releases: None,
        readme: None,
        latest_commit: None,
        permalink: String::new(),
        references: Default::default(),
        empty: loaded.refs.is_empty(),
        http_clone: format!(
            "{}/{}/{}.git",
            crate::backend::api::http::config::http_clone_base().trim_end_matches('/'),
            loaded.chrome.owner,
            loaded.chrome.name
        ),
        ssh_clone: {
            let base = crate::backend::api::http::config::ssh_clone_base();
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
        if !super::super::chrome::is_branch(loaded, &reference) && !reference.starts_with("refs/") {
            page.reference = last.sha.clone();
        }
        // The size check and blob read must refer to the same commit, even during a push.
        let entries = match store.tree(&last.sha, &resolved.path).await {
            Ok(entries) => entries,
            Err(error) => {
                return match root_if_missing(ctx, loaded, &reference, &error) {
                    Some(redirect) => Ok(redirect),
                    None => Err(fail(error)),
                };
            }
        };
        // A folder's header shows the newest commit that touched its path, including commits
        // that only removed entries. The root has no path to scope to and keeps the branch head.
        let folder_commit = if resolved.path.is_empty() {
            Some(last.clone())
        } else {
            match store
                .path_commits(&last.sha, &resolved.path, 0, 1, &Default::default())
                .await
            {
                Ok(commits) => commits.into_iter().next(),
                Err(error) if error.is_response_too_large() => None,
                Err(error) => return Err(fail(error)),
            }
        };
        let readme_entry = entries.iter().find(|entry| {
            entry.entry_type == "file" && entry.name.eq_ignore_ascii_case("readme.md")
        });
        let readme = match readme_entry {
            Some(entry)
                if entry
                    .size
                    .is_some_and(|size| size <= view::tree::MAX_README_BYTES) =>
            {
                Some(store.blob(&last.sha, &entry.path).await.map_err(fail)?)
            }
            _ => None,
        };
        // The Files panel shows the folders on the way down to this one, open, so each is listed.
        // The repository root has no Files panel; its toolbar searches `entries`.
        let listing = view_entries(entries);
        if !resolved.path.is_empty() {
            let parent = resolved
                .path
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent);
            let mut folders = super::blob::folder_listings(&store, &last.sha, parent)
                .await
                .map_err(fail)?;
            folders.push(view::blob::FolderListing {
                path: resolved.path.clone(),
                entries: listing.clone(),
            });
            page.folders = folders;
        }
        page.entries = listing;
        page.permalink = format!(
            "{}{}",
            crate::backend::api::http::config::public_origin(),
            view::ref_path::file_url(&loaded.chrome.base(), "tree", &last.sha, &resolved.path)
        );
        page.latest_commit = folder_commit.map(commit);
        if resolved.path.is_empty() {
            page.languages = about_languages(
                &ctx.state,
                loaded,
                &store,
                branch_name(&default_ref(loaded)),
            )
            .await;
            let viewer = ctx.user.as_ref().map(|user| user.id);
            page.releases = releases::summary(&ctx.state.pool, viewer, &loaded.repository)
                .await
                .ok()
                .map(|summary| view::releases::ReleasesSummary {
                    count: summary.count,
                    latest_tag: summary.latest.map(|release| release.tag_name),
                });
        }
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
        crate::webui::app::Page::Repositories(view::page::Page::Tree {
            repo: loaded.chrome.clone(),
            page,
        }),
    ))
}

/// The About panel's language bar for the default branch: the stored languages, or the computed
/// ones when none are stored yet. The panel is secondary, so a failure leaves the bar out rather
/// than failing the page.
async fn about_languages(
    state: &AppState,
    loaded: &Loaded,
    store: &RepoStore,
    branch: &str,
) -> Vec<LanguageShare> {
    let stored = repo_languages::stored(&state.pool, loaded.repository.id)
        .await
        .unwrap_or_default();
    let languages = if stored.is_empty() {
        match repo_languages::compute_and_store(&state.pool, store, &loaded.repository, branch)
            .await
        {
            Ok(languages) => languages,
            Err(error) => {
                tracing::warn!(repo_id = loaded.repository.id, %error, "could not compute the language bar");
                Vec::new()
            }
        }
    } else {
        stored
    };
    languages
        .into_iter()
        .map(|language| LanguageShare {
            name: language.language,
            percentage: f64::from(language.percentage),
            color: language.color,
        })
        .collect()
}

pub(in crate::backend::api::repos) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx =
        match crate::backend::api::http::context::PageContext::load(state, headers, &uri).await {
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
