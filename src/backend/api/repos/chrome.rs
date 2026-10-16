use super::access::check_repo_read_access;
use crate::backend::api::context::PageContext;
use crate::backend::db;
use crate::backend::repositories::lifecycle;
use crate::backend::repositories::store::{
    repo_store, CommitInfo, ErrorKind, RepoError, MAX_COMMIT_PAGE_SIZE,
};
use crate::webui::pages::repositories as view;
use crate::webui::pages::repositories::ref_path::{resolve_repo_ref_path, split_path};
use crate::webui::pages::repositories::RepoChrome;
use axum::http::StatusCode;
use axum::response::Response;
use chrono::TimeZone;
use chrono::Utc;
use std::collections::HashMap;
pub(crate) struct Loaded {
    pub(crate) repository: db::models::Repository,
    pub(crate) chrome: RepoChrome,
    /// Empty when `git_unavailable` is set.
    pub(crate) refs: HashMap<String, String>,
    /// The stored branch when `git_unavailable` is set.
    pub(crate) default_branch: String,
    pub(crate) git_unavailable: bool,
}

pub(crate) fn unavailable(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Repository data is temporarily unavailable.",
    )
}

/// The answer for a page or action that needs Git while its storage is unavailable.
pub(crate) fn git_unavailable_response(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Git storage is temporarily unavailable.",
    )
}

pub(crate) fn api_failure(ctx: &PageContext, status: StatusCode) -> Response {
    let message = safe_error(status);
    ctx.error(status, message)
}

const MISSING_REVISION: &str = "Check the path or revision, or pick another branch, tag or commit.";

pub(crate) fn safe_error(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "Check the address, or search for the repository.",
        StatusCode::UNAUTHORIZED => "Sign in to perform this action.",
        StatusCode::FORBIDDEN => "You do not have permission to perform this action.",
        StatusCode::CONFLICT => "The operation conflicts with existing data. Review your submission before retrying.",
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "The submitted values could not be accepted. Check names, source revisions and required fields.",
        StatusCode::PAYLOAD_TOO_LARGE => "The submitted content exceeds the supported size.",
        _ => "The operation could not be confirmed because repository services are unavailable.",
    }
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn from_api<T>(ctx: &PageContext, result: ApiResult<T>) -> Result<T, Response> {
    result.map_err(|(status, _)| api_failure(ctx, status))
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

/// Repository data for pages and actions that need only the database. Git enrichment is
/// optional here: when storage is unavailable, the stored default branch stands in and no
/// branches or tags are listed.
pub(crate) async fn load_metadata(
    ctx: &PageContext,
    owner: &str,
    name: &str,
) -> Result<Loaded, Response> {
    let repository = db::repositories::records::get_repository(&ctx.state.pool, owner, name)
        .await
        .map_err(|_| unavailable(ctx))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
    from_api(
        ctx,
        check_repo_read_access(&ctx.state.pool, &repository, &ctx.headers).await,
    )?;
    let mut chrome = RepoChrome {
        id: repository.id,
        owner: owner.to_owned(),
        name: repository.name.clone(),
        description: repository.description.clone().unwrap_or_default(),
        is_private: repository.is_private,
        is_archived: repository.is_archived,
        stars: repository.stars_count,
        watchers: repository.watchers_count,
        forks: repository.forks_count,
        csrf_token: ctx.csrf_token.clone(),
        current_path: ctx.path.clone(),
        signed_in: ctx.user.is_some(),
        viewer_id: ctx.user.as_ref().map_or(0, |user| user.id),
        ..RepoChrome::default()
    };
    if let Some(user) = &ctx.user {
        chrome.is_owner = repository.is_personal_owner(user.id);
        chrome.can_manage_collaborators = repository.is_personal_owner(user.id);
        chrome.fork_refusal =
            lifecycle::fork_refusal(&repository, user.id).map(|error| error.message.to_owned());
        if chrome.is_owner || (user.is_admin && repository.owner_kind == "user") {
            chrome.can_write = true;
            chrome.can_admin = true;
        } else {
            let access = db::repositories::access::get_user_repo_access(
                &ctx.state.pool,
                user.id,
                repository.id,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            chrome.is_owner = access.is_owner();
            chrome.can_write = access.can_write();
            chrome.can_admin = access.can_admin();
        }
        chrome.starred =
            db::repositories::engagement::is_repo_starred(&ctx.state.pool, user.id, repository.id)
                .await
                .map_err(|_| unavailable(ctx))?;
        chrome.watching =
            db::repositories::engagement::is_repo_watched(&ctx.state.pool, user.id, repository.id)
                .await
                .map_err(|_| unavailable(ctx))?;
        if let Some(fork) =
            db::repositories::discovery::get_user_fork(&ctx.state.pool, user.id, repository.id)
                .await
                .map_err(|_| unavailable(ctx))?
        {
            // Only the actor's own fork is disclosed.
            chrome.fork_url = Some(format!(
                "/{}/{}",
                crate::webui::encode_component(&user.username),
                crate::webui::encode_component(&fork.name)
            ));
        }
    }
    if let Some(parent_id) = repository.forked_from_id {
        if let Some(parent) =
            db::repositories::records::get_repository_by_id(&ctx.state.pool, parent_id)
                .await
                .map_err(|_| unavailable(ctx))?
        {
            match check_repo_read_access(&ctx.state.pool, &parent, &ctx.headers).await {
                Ok(()) => {
                    let parent_owner = db::repositories::records::repository_owner_name(
                        &ctx.state.pool,
                        parent.id,
                    )
                    .await
                    .map_err(|_| unavailable(ctx))?
                    .ok_or_else(|| unavailable(ctx))?;
                    chrome.parent_name = Some(format!("{}/{}", parent_owner, parent.name));
                    chrome.parent_url = Some(format!(
                        "/{}/{}",
                        crate::webui::encode_component(&parent_owner),
                        crate::webui::encode_component(&parent.name)
                    ));
                }
                Err((StatusCode::NOT_FOUND, _)) => {} // Do not disclose inaccessible fork ancestry.
                Err((status, _)) => return Err(api_failure(ctx, status)),
            }
        }
    }
    if repository.is_archived {
        chrome.can_write = false;
    }
    let git = crate::backend::repositories::reads::refs_and_default(
        &repository,
        ctx.state.cluster.as_ref(),
    )
    .await;
    let (refs, default_branch, git_unavailable) = match git {
        Ok((refs, default_branch)) => (refs, default_branch, false),
        Err(error) if error.kind == ErrorKind::Unavailable => {
            (HashMap::new(), repository.default_branch.clone(), true)
        }
        Err(error) => return Err(store_error(ctx, Some(&chrome), error)),
    };
    Ok(Loaded {
        repository,
        chrome,
        refs,
        default_branch,
        git_unavailable,
    })
}

/// Shared repository chrome for the collaboration/actions page owners.
pub async fn load_chrome(
    ctx: &PageContext,
    owner: &str,
    name: &str,
) -> Result<RepoChrome, Response> {
    Ok(load_metadata(ctx, owner, name).await?.chrome)
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
        .collect();
    names.sort();
    names.dedup();
    names
}

pub(crate) fn default_ref(loaded: &Loaded) -> String {
    let stored = &loaded.default_branch;
    let branch = crate::backend::repositories::reads::default_branch(&loaded.refs, stored, stored);
    if branch != *stored
        || loaded.refs.is_empty()
        || loaded.refs.contains_key(&format!("refs/heads/{stored}"))
    {
        return branch;
    }
    ref_names(loaded)
        .into_iter()
        .next()
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
    if reference.starts_with('-')
        || (!hex_revision(reference) && !ref_names(loaded).iter().any(|name| name == reference))
    {
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
) -> Result<view::ref_path::ResolvedRefPath, Response> {
    let resolved = resolve_repo_ref_path(&split_path(tail), &ref_names(loaded), require_path)
        .ok_or_else(|| {
            ctx.error(
                StatusCode::NOT_FOUND,
                "A reference and file path are required.",
            )
        })?;
    if !loaded.refs.is_empty() {
        ensure_ref(ctx, loaded, &resolved.reference)?;
    }
    Ok(resolved)
}

pub(crate) fn date(timestamp: i64) -> String {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|date| date.to_rfc3339())
        .unwrap_or_else(|| "Unknown date".into())
}

pub(crate) fn commit(commit: CommitInfo) -> view::Commit {
    view::Commit {
        sha: commit.sha,
        message: commit.message,
        author: commit.author_name,
        author_date: date(commit.timestamp),
        committer: commit.committer_name,
        committer_date: date(commit.committer_timestamp),
        parents: commit.parents,
    }
}

/// Shown when Git has no such path or revision. `repo` is `None` where the page has no
/// repository header to show.
pub(crate) fn store_error(
    ctx: &PageContext,
    repo: Option<&RepoChrome>,
    error: RepoError,
) -> Response {
    if error.is_response_too_large() {
        return ctx.error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Repository response exceeds its limit.",
        );
    }
    if error.is_storage_node_unavailable() {
        return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "storage node unavailable");
    }
    if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) {
        match repo {
            Some(repo) => ctx.repo_error(repo, StatusCode::NOT_FOUND, MISSING_REVISION),
            None => ctx.error(StatusCode::NOT_FOUND, MISSING_REVISION),
        }
    } else {
        unavailable(ctx)
    }
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn number(
    ctx: &PageContext,
    key: &str,
    default: usize,
    max: usize,
) -> Result<usize, Response> {
    match ctx.query.get(key) {
        None => Ok(default),
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value <= max)
            .ok_or_else(|| {
                ctx.error(
                    StatusCode::BAD_REQUEST,
                    "Invalid pagination or preview offset.",
                )
            }),
    }
}

/// Items per page for HTML lists and the default `per_page` for JSON lists.
pub(crate) const DEFAULT_PER_PAGE: usize = 30;
pub(crate) const MAX_PAGE: usize = 1_000;

#[derive(serde::Deserialize)]
pub(crate) struct PageQuery {
    pub(crate) page: Option<u32>,
    pub(crate) per_page: Option<u32>,
}

fn page_error() -> (StatusCode, axum::Json<crate::backend::api::error::ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(crate::backend::api::error::ApiError::new(
            "validation_error",
            "Page must be between 1 and 1000",
        )),
    )
}

fn page_offset(page: usize, limit: usize) -> Option<usize> {
    page.checked_sub(1)?.checked_mul(limit)
}

/// SQL offset and limit for a 1-based page. The limit is clamped to the store's
/// page size.
pub(crate) fn page_window(page: usize, per_page: usize) -> Option<(usize, usize)> {
    if !(1..=MAX_PAGE).contains(&page) {
        return None;
    }
    let limit = per_page.clamp(1, MAX_COMMIT_PAGE_SIZE);
    Some((page_offset(page, limit)?, limit))
}

/// Whether a page after `page` exists. `fetched` is the row count read for `page`
/// with one extra row, so a full extra row means more rows follow. The cap is the
/// last page a URL can name, so it never offers a next page.
pub(crate) fn has_next_page(page: usize, fetched: usize, per_page: usize) -> bool {
    fetched > per_page && page < MAX_PAGE
}

/// JSON list paging: `page` is 1 through 1,000, `per_page` defaults to 30 and is
/// clamped to 1 through 100. Returns the offset and limit.
pub(crate) fn json_page(
    page: Option<u32>,
    per_page: Option<u32>,
) -> Result<(usize, usize), (StatusCode, axum::Json<crate::backend::api::error::ApiError>)> {
    let page = usize::try_from(page.unwrap_or(1)).map_err(|_| page_error())?;
    let per_page =
        usize::try_from(per_page.unwrap_or(DEFAULT_PER_PAGE as u32)).map_err(|_| page_error())?;
    page_window(page, per_page).ok_or_else(page_error)
}

pub(crate) type ApiResult<T> =
    Result<T, (StatusCode, axum::Json<crate::backend::api::error::ApiError>)>;

pub(crate) async fn load_refs_from_repo(
    state: &crate::backend::app::AppState,
    repository: &db::models::Repository,
    _repo_id: &str,
) -> ApiResult<HashMap<String, String>> {
    repo_store(repository, state.cluster.as_ref())
        .refs()
        .await
        .map_err(|error| super::responses::repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE))
}

/// Metadata may use the stored branch when optional Git enrichment is unavailable;
/// a bounded read refusal remains explicit rather than looking like valid metadata.
pub(crate) async fn repository_default_branch_or_db(
    state: &crate::backend::app::AppState,
    repository: &db::models::Repository,
) -> ApiResult<String> {
    match repo_store(repository, state.cluster.as_ref()).info().await {
        Ok(info) => Ok(info.default_branch),
        Err(error) if error.is_response_too_large() => Err(super::responses::repo_store_error(
            error,
            StatusCode::SERVICE_UNAVAILABLE,
        )),
        Err(_) => Ok(repository.default_branch.clone()),
    }
}

pub(crate) async fn repository_default_branch(
    state: &crate::backend::app::AppState,
    repository: &db::models::Repository,
) -> ApiResult<String> {
    repo_store(repository, state.cluster.as_ref())
        .info()
        .await
        .map(|info| info.default_branch)
        .map_err(|error| super::responses::repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_use_an_offset_and_clamp_page_size() {
        assert_eq!(page_window(1, 30), Some((0, 30)));
        assert_eq!(page_window(2, 30), Some((30, 30)));
        assert_eq!(
            page_window(2, MAX_COMMIT_PAGE_SIZE),
            Some((MAX_COMMIT_PAGE_SIZE, MAX_COMMIT_PAGE_SIZE))
        );
        assert_eq!(
            page_window(2, MAX_COMMIT_PAGE_SIZE + 1),
            Some((MAX_COMMIT_PAGE_SIZE, MAX_COMMIT_PAGE_SIZE))
        );
        assert_eq!(page_window(1, 0), Some((0, 1)));
    }

    #[test]
    fn pages_reject_out_of_range_and_overflowing_offsets() {
        assert_eq!(page_window(0, 30), None);
        assert_eq!(page_window(MAX_PAGE, 30), Some((29_970, 30)));
        assert_eq!(page_window(MAX_PAGE + 1, 30), None);
        assert_eq!(page_offset(usize::MAX, MAX_COMMIT_PAGE_SIZE), None);
    }

    #[test]
    fn next_page_needs_an_extra_row_and_stops_at_the_cap() {
        assert!(has_next_page(1, 31, 30));
        assert!(!has_next_page(1, 30, 30));
        assert!(has_next_page(MAX_PAGE - 1, 31, 30));
        assert!(!has_next_page(MAX_PAGE, 31, 30));
    }
}
