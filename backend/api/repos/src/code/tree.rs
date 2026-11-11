use crate::access::readable_repository;
use crate::chrome::branch_name;
use crate::chrome::default_ref;
use crate::chrome::ref_names_under;
use crate::chrome::resolve;
use crate::chrome::root_if_missing;
use crate::refs::references::resolve_references;
use crate::responses::CommitResponse;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::shared::repos::access::check_repo_read_access;
use codeyard_api_http::shared::repos::chrome::commit;
use codeyard_api_http::shared::repos::chrome::store_error;
use codeyard_api_http::shared::repos::chrome::Loaded;
use codeyard_app::app::AppState;
use codeyard_git::TreeEntry;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_git_storage::repositories::store::RepoStore;
use codeyard_webui::components::languages::LanguageShare;
use codeyard_webui::pages::repos as view;
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

/// The entries of the folder at `path`, resolved directly from the requested revision.
async fn tree_entries(
    state: &AppState,
    headers: &HeaderMap,
    (owner, repo): (&str, &str),
    git_ref: &str,
    path: &str,
) -> Result<Json<Vec<TreeEntryResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, owner, repo, headers).await?;

    let tree = repo_store(&repository, state.cluster.as_ref())
        .tree(git_ref, path)
        .await
        .map_err(|e| crate::responses::repo_store_error(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    check_repo_read_access(&state.pool, &repository, headers).await?;
    Ok(Json(
        tree.into_iter().map(TreeEntryResponse::from).collect(),
    ))
}

async fn tree_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    let resolved = if tail.is_empty() {
        codeyard_types::ref_path::ResolvedRefPath {
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
        migration_status: codeyard_db_repos::source_imports::get(
            &ctx.state.pool,
            loaded.repository.id,
        )
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Migration status unavailable.",
            )
        })?
        .and_then(|import| (import.state() != "complete").then(|| import.state().to_owned())),
        http_clone: format!(
            "{}/{}/{}.git",
            codeyard_types::http_config::http_clone_base().trim_end_matches('/'),
            loaded.chrome.owner,
            loaded.chrome.name
        ),
        ssh_clone: {
            let base = codeyard_types::http_config::ssh_clone_base();
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
        if !crate::chrome::is_branch(loaded, &reference) && !reference.starts_with("refs/") {
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
            let mut folders = crate::code::blob::folder_listings(&store, &last.sha, parent)
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
            codeyard_types::http_config::public_origin(),
            codeyard_types::ref_path::file_url(
                &loaded.chrome.base(),
                "tree",
                &last.sha,
                &resolved.path
            )
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
            page.releases = codeyard_repositories::repositories::releases::summary(
                &ctx.state.pool,
                viewer,
                &loaded.repository,
            )
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
    check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers)
        .await
        .map_err(|(status, _)| ctx.error(status, "Repository not found."))?;
    Ok(ctx.page_model(
        &format!("{}/{}", loaded.chrome.owner, loaded.chrome.name),
        codeyard_webui::app::Page::Repositories(view::page::Page::Tree {
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
    let languages = match codeyard_git_storage::repositories::languages::compute_and_store(
        &state.pool,
        store,
        &loaded.repository,
        branch,
    )
    .await
    {
        Ok(languages) => languages,
        Err(error) => {
            tracing::warn!(repo_id = loaded.repository.id, %error, "could not compute the language bar");
            Vec::new()
        }
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

pub(crate) async fn page(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<codeyard_app::app::AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    crate::code::GitPathParams(params): crate::code::GitPathParams,
) -> axum::response::Response {
    let ctx = match codeyard_api_http::context::PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match crate::chrome::load(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    match tree_page(&ctx, &loaded, tail).await {
        Ok(response) | Err(response) => response,
    }
}
