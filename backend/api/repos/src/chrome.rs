use axum::http::StatusCode;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_types::ref_path::resolve_repo_ref_path;
use codeyard_types::ref_path::split_path;
use std::collections::HashMap;

/// The answer for a page or action that needs Git while its storage is unavailable.
pub(crate) fn git_unavailable_response(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Git storage is temporarily unavailable.",
    )
}

/// Repository data for pages that read Git. Unavailable storage is a 503 here, so these pages
/// never show an empty branch, tag or file list in place of the real one.
pub(crate) async fn load(ctx: &PageContext, owner: &str, name: &str) -> Result<Loaded, Response> {
    let loaded = load_metadata(ctx, owner, name).await?;
    if loaded.git_unavailable {
        return Err(git_unavailable_response(ctx));
    }
    Ok(loaded)
}

pub(crate) fn ref_names(loaded: &Loaded) -> Vec<String> {
    let mut names: Vec<_> = loaded
        .refs
        .keys()
        .filter_map(|name| {
            name.strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/tags/"))
        })
        .map(str::to_owned)
        .chain(loaded.refs.keys().cloned())
        .collect();
    if loaded.refs.is_empty() {
        names.push(loaded.default_branch.clone());
        names.push(format!("refs/heads/{}", loaded.default_branch));
    }
    names.sort();
    names.dedup();
    names
}

/// Whether `reference` is a branch. Only a branch takes a commit from the file pages, so only a
/// branch offers edit and delete there.
pub(crate) fn is_branch(loaded: &Loaded, reference: &str) -> bool {
    if codeyard_types::git_ids::is_full_commit_id(reference) {
        return false;
    }
    if reference.starts_with("refs/") {
        reference.starts_with("refs/heads/") && loaded.refs.contains_key(reference)
    } else {
        loaded.refs.contains_key(&format!("refs/heads/{reference}"))
    }
}

/// Mutation APIs take branch names, while Git reads keep fully qualified refs.
pub(crate) fn branch_name(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

/// The names under one ref prefix, `refs/heads/` or `refs/tags/`, sorted.
pub(crate) fn ref_names_under(loaded: &Loaded, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = loaded
        .refs
        .keys()
        .filter_map(|name| name.strip_prefix(prefix))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

pub(crate) fn default_ref(loaded: &Loaded) -> String {
    let stored = &loaded.default_branch;
    let branch =
        codeyard_git_storage::repositories::reads::default_branch(&loaded.refs, stored, stored);
    if branch != *stored
        || loaded.refs.is_empty()
        || loaded.refs.contains_key(&format!("refs/heads/{stored}"))
    {
        return if branch.starts_with("refs/") || hex_revision(&branch) {
            format!("refs/heads/{branch}")
        } else {
            branch
        };
    }
    loaded
        .refs
        .keys()
        .min()
        .cloned()
        .unwrap_or_else(|| stored.clone())
}

pub(crate) fn hex_revision(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn ensure_ref(
    ctx: &PageContext,
    loaded: &Loaded,
    reference: &str,
) -> Result<(), Response> {
    // The store resolves a known ref before Git sees it, so a name that starts with a hyphen is safe.
    if !hex_revision(reference) && !ref_names(loaded).iter().any(|name| name == reference) {
        Err(ctx.repo_error(&loaded.chrome, StatusCode::NOT_FOUND, MISSING_REVISION))
    } else {
        Ok(())
    }
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn resolve(
    ctx: &PageContext,
    loaded: &Loaded,
    tail: &str,
    require_path: bool,
) -> Result<codeyard_types::ref_path::ResolvedRefPath, Response> {
    if tail.len() > 4096 {
        return Err(ctx.error(StatusCode::URI_TOO_LONG, "Repository path is too long."));
    }
    let names = if loaded.refs.is_empty() {
        vec![
            loaded.default_branch.clone(),
            format!("refs/heads/{}", loaded.default_branch),
        ]
    } else if tail.starts_with("refs/heads/") || tail.starts_with("refs/tags/") {
        loaded.refs.keys().cloned().collect()
    } else {
        ref_names(loaded)
    };
    let mut resolved =
        resolve_repo_ref_path(&split_path(tail), &names, require_path).ok_or_else(|| {
            ctx.error(
                StatusCode::NOT_FOUND,
                "A reference and file path are required.",
            )
        })?;
    if loaded.refs.is_empty() {
        if codeyard_git::ref_names::branch_ref(&resolved.reference).is_none()
            || (resolved.reference != loaded.default_branch
                && resolved.reference != format!("refs/heads/{}", loaded.default_branch))
        {
            return Err(ctx.repo_error(&loaded.chrome, StatusCode::NOT_FOUND, MISSING_REVISION));
        }
    } else {
        ensure_ref(ctx, loaded, &resolved.reference)?;
    }
    if resolved.reference.starts_with("refs/")
        && !loaded.refs.contains_key(&resolved.reference)
        && loaded
            .refs
            .contains_key(&format!("refs/heads/{}", resolved.reference))
    {
        resolved.reference = format!("refs/heads/{}", resolved.reference);
    } else if !resolved.reference.starts_with("refs/")
        && !is_branch(loaded, &resolved.reference)
        && loaded
            .refs
            .contains_key(&format!("refs/tags/{}", resolved.reference))
    {
        resolved.reference = format!("refs/tags/{}", resolved.reference);
    }
    Ok(resolved)
}

/// A ref switch asks for the same path on another ref. When that path is missing there, the
/// answer is a redirect to the ref's root; any other failure stays an error.
pub(crate) fn root_if_missing(
    ctx: &PageContext,
    loaded: &Loaded,
    reference: &str,
    error: &RepoError,
) -> Option<Response> {
    let switching = ctx
        .query
        .contains_key(codeyard_types::ref_path::ROOT_IF_MISSING);
    let missing = matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath);
    (switching && missing).then(|| {
        let root = codeyard_types::ref_path::file_url(&loaded.chrome.base(), "tree", reference, "");
        codeyard_api_http::render::redirect_response(&root)
    })
}

pub(crate) async fn load_refs_from_repo(
    state: &codeyard_app::app::AppState,
    repository: &codeyard_database::models::Repository,
) -> ApiResult<HashMap<String, String>> {
    repo_store(repository, state.cluster.as_ref())
        .refs()
        .await
        .map_err(|error| crate::responses::repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE))
}

/// Metadata may use the stored branch when optional Git enrichment is unavailable.
pub(crate) async fn repository_default_branch_or_db(
    state: &codeyard_app::app::AppState,
    repository: &codeyard_database::models::Repository,
) -> String {
    match repo_store(repository, state.cluster.as_ref()).info().await {
        Ok(info) => info.default_branch,
        Err(_) => repository.default_branch.clone(),
    }
}

pub(crate) async fn repository_default_branch(
    state: &codeyard_app::app::AppState,
    repository: &codeyard_database::models::Repository,
) -> ApiResult<String> {
    repo_store(repository, state.cluster.as_ref())
        .info()
        .await
        .map(|info| info.default_branch)
        .map_err(|error| crate::responses::repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE))
}

use codeyard_api_http::shared::repos::chrome::ApiResult;

use codeyard_api_http::shared::repos::chrome::Loaded;

use codeyard_api_http::shared::repos::chrome::MISSING_REVISION;

use codeyard_api_http::shared::repos::chrome::load_metadata;
