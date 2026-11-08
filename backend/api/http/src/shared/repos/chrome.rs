use crate::context::PageContext;
use crate::shared::repos::access::check_repo_read_access;
use crate::shared::repos::responses::rfc3339;
use axum::http::StatusCode;
use axum::response::Response;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::CommitInfo;
use codeyard_webui::pages::repos as view;
use codeyard_webui::pages::repos::chrome::RepoChrome;
use std::collections::HashMap;
pub struct Loaded {
    pub repository: codeyard_db_core::models::Repository,
    pub chrome: RepoChrome,
    /// Empty when `git_unavailable` is set.
    pub refs: HashMap<String, String>,
    /// The stored branch when `git_unavailable` is set.
    pub default_branch: String,
    pub git_unavailable: bool,
}

pub fn unavailable(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Repository data is temporarily unavailable.",
    )
}

pub fn api_failure(ctx: &PageContext, status: StatusCode) -> Response {
    let message = safe_error(status);
    ctx.error(status, message)
}

pub const MISSING_REVISION: &str =
    "Check the path or revision, or pick another branch, tag or commit.";

pub fn safe_error(status: StatusCode) -> &'static str {
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

/// Database chrome with optional Git enrichment for pages that display refs.
pub async fn load_metadata(ctx: &PageContext, owner: &str, name: &str) -> Result<Loaded, Response> {
    let mut loaded = load_database(ctx, owner, name).await?;
    let git = codeyard_repos_core::repositories::reads::refs_and_default(
        &loaded.repository,
        ctx.state.cluster.as_ref(),
    )
    .await;
    match git {
        Ok((refs, default_branch)) => {
            loaded.refs = refs;
            loaded.chrome.default_branch = default_branch.clone();
            loaded.default_branch = default_branch;
        }
        Err(error) if error.kind == ErrorKind::Unavailable => loaded.git_unavailable = true,
        Err(error) => return Err(store_error(ctx, Some(&loaded.chrome), error)),
    }
    Ok(loaded)
}

/// Database-only actions must not wait for Git storage.
pub async fn load_database(ctx: &PageContext, owner: &str, name: &str) -> Result<Loaded, Response> {
    let repository =
        codeyard_db_repos::repositories::records::get_repository(&ctx.state.pool, owner, name)
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
    from_api(
        ctx,
        check_repo_read_access(&ctx.state.pool, &repository, &ctx.headers).await,
    )?;
    let topics = codeyard_db_repos::repositories::topics::repository_topics(
        &ctx.state.pool,
        &[repository.id],
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .remove(&repository.id)
    .unwrap_or_default();
    let forks = from_api(
        ctx,
        crate::shared::repos::responses::repository_fork_counts(
            &ctx.state.pool,
            Some(&ctx.headers),
            &[repository.id],
        )
        .await,
    )?
    .remove(&repository.id)
    .unwrap_or_default();
    let mut chrome = RepoChrome {
        id: repository.id,
        owner: owner.to_owned(),
        name: repository.name.clone(),
        description: repository.description.clone().unwrap_or_default(),
        website: repository.website.clone().unwrap_or_default(),
        topics,
        is_private: repository.is_private,
        is_archived: repository.is_archived,
        is_template: repository.is_template,
        stars: repository.stars_count,
        watchers: repository.watchers_count,
        forks,
        csrf_token: ctx.csrf_token.clone(),
        current_path: if owner.eq_ignore_ascii_case("gists")
            && !ctx.path.starts_with("/gists/@repos/")
        {
            format!("/gists/@repos{}", ctx.path)
        } else {
            ctx.path.clone()
        },
        signed_in: ctx.user.is_some(),
        viewer_id: ctx.user.as_ref().map_or(0, |user| user.id),
        ..RepoChrome::default()
    };
    let package_session = ctx
        .user
        .as_ref()
        .and_then(|_| crate::session::extract_session_id(&ctx.headers));
    chrome.packages = codeyard_db_repos::packages::count_linked(
        &ctx.state.pool,
        package_session
            .as_deref()
            .map(codeyard_db_repos::packages::Credential::Session),
        repository.id,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    // The Issues and Pull requests tabs show open counts, and every reader of the repository sees them.
    chrome.open_issues = codeyard_db_repos::issues::count_repo_issues(
        &ctx.state.pool,
        repository.id,
        &Default::default(),
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .open;
    chrome.open_pulls = codeyard_db_repos::pull_requests::records::count_repo_pull_requests(
        &ctx.state.pool,
        repository.id,
        &Default::default(),
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .open;
    if let Some(user) = &ctx.user {
        chrome.is_owner = repository.is_personal_owner(user.id);
        chrome.can_manage_collaborators = chrome.is_owner;
        chrome.fork_refusal =
            codeyard_repos_core::repositories::lifecycle::fork_refusal(&repository, user.id)
                .map(|error| error.message.to_owned());
        if !chrome.is_owner {
            chrome.is_owner = codeyard_db_repos::repositories::access::get_user_repo_access(
                &ctx.state.pool,
                user.id,
                repository.id,
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .is_owner();
        }
        let effective_access =
            codeyard_identity::repository_access::user_access(&ctx.state.pool, user, &repository)
                .await
                .map_err(|_| unavailable(ctx))?;
        chrome.can_write =
            codeyard_identity::repository_access::can_write(&ctx.state.pool, user, &repository)
                .await
                .map_err(|_| unavailable(ctx))?;
        chrome.can_admin = effective_access.can_admin();
        chrome.can_view_traffic = effective_access.can_write();
        chrome.can_manage_invitations = chrome.can_admin;
        chrome.starred = codeyard_db_repos::repositories::engagement::is_repo_starred(
            &ctx.state.pool,
            user.id,
            repository.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?;
        if let Some((level, events)) = codeyard_db_repos::repositories::engagement::get_watch_level(
            &ctx.state.pool,
            user.id,
            repository.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        {
            chrome.watch_level = level;
            chrome.watch_events = events;
        }
        if let Some(fork) = codeyard_db_repos::repositories::discovery::get_user_fork(
            &ctx.state.pool,
            user.id,
            repository.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        {
            // Only the actor's own fork is disclosed.
            chrome.fork_url = Some(codeyard_types::paths::repository_path(
                &user.username,
                &fork.name,
            ));
        }
    }
    if let Some(parent_id) = repository.forked_from_id {
        if let Some(parent) = codeyard_db_repos::repositories::records::get_repository_by_id(
            &ctx.state.pool,
            parent_id,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        {
            match check_repo_read_access(&ctx.state.pool, &parent, &ctx.headers).await {
                Ok(()) => {
                    let parent_owner =
                        codeyard_db_repos::repositories::records::repository_owner_name(
                            &ctx.state.pool,
                            parent.id,
                        )
                        .await
                        .map_err(|_| unavailable(ctx))?
                        .ok_or_else(|| unavailable(ctx))?;
                    chrome.parent_name = Some(format!("{}/{}", parent_owner, parent.name));
                    chrome.parent_url = Some(codeyard_types::paths::repository_path(
                        &parent_owner,
                        &parent.name,
                    ));
                }
                Err((StatusCode::NOT_FOUND, _)) => {} // Do not disclose inaccessible fork ancestry.
                Err((status, _)) => return Err(api_failure(ctx, status)),
            }
        }
    }
    // Actor-scoped metadata (including a private personal fork link) must not outlive
    // the browser credential that selected it, even when the source remains public.
    if let Some(expected) = &ctx.user {
        let current = from_api(
            ctx,
            crate::session::get_optional_user(&ctx.state.pool, &ctx.headers).await,
        )?;
        if !current.is_some_and(|user| user.id == expected.id) {
            return Err(ctx.error(
                StatusCode::UNAUTHORIZED,
                "Session expired; reload the page.",
            ));
        }
    }
    from_api(
        ctx,
        check_repo_read_access(&ctx.state.pool, &repository, &ctx.headers).await,
    )?;
    let default_branch = repository.default_branch.clone();
    chrome.default_branch = default_branch.clone();
    Ok(Loaded {
        repository,
        chrome,
        refs: HashMap::new(),
        default_branch,
        git_unavailable: false,
    })
}

pub fn date(timestamp: i64) -> String {
    rfc3339(timestamp).unwrap_or_else(|| "Unknown date".into())
}

pub fn commit(commit: CommitInfo) -> view::commits::Commit {
    view::commits::Commit {
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
pub fn store_error(ctx: &PageContext, repo: Option<&RepoChrome>, error: RepoError) -> Response {
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

pub type ApiResult<T> = Result<T, (StatusCode, axum::Json<crate::error::ApiError>)>;
