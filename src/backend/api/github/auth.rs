//! Token-only authentication, separate from browser sessions and CSRF.
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;

use crate::backend::app::AppState;

use axum::http::{header, HeaderMap, Method, StatusCode};
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_core::models::User;
use codeyard_support::token_hash::token_hash;

#[derive(Clone)]
pub(in crate::backend::api) struct Authentication {
    pub token: PersonalAccessToken,
    pub user: User,
}

struct RequestContext {
    authorization: Vec<axum::http::HeaderValue>,
    authentication: tokio::sync::OnceCell<Option<Authentication>>,
    public_only: std::sync::atomic::AtomicBool,
}

tokio::task_local! {
    static REQUEST_CONTEXT: std::sync::Arc<RequestContext>;
}

/// Header-based adapters share one verified identity without letting a public-only audience
/// borrow account authority. The scope is request-local and is not inherited by spawned tasks.
pub(super) async fn request_context(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let context = RequestContext {
        authorization: request
            .headers()
            .get_all(header::AUTHORIZATION)
            .iter()
            .cloned()
            .collect(),
        authentication: tokio::sync::OnceCell::new(),
        public_only: std::sync::atomic::AtomicBool::new(false),
    };
    REQUEST_CONTEXT
        .scope(std::sync::Arc::new(context), next.run(request))
        .await
}

fn context(headers: &HeaderMap) -> Option<std::sync::Arc<RequestContext>> {
    let context = REQUEST_CONTEXT.try_with(std::sync::Arc::clone).ok()?;
    context
        .authorization
        .iter()
        .eq(headers.get_all(header::AUTHORIZATION).iter())
        .then_some(context)
}

#[derive(Debug)]
pub(in crate::backend::api) enum ScopeError {
    Missing,
    Denied,
    Unavailable,
}

/// Recheck the original REST credential and repository scope after buffering Git data.
/// Cluster-node authorization cannot stand in for this reader's authority.
pub(super) async fn recheck_repository_read(
    state: &AppState,
    headers: &HeaderMap,
    repository: i64,
    scope: &str,
) -> Result<()> {
    let mut auth = authenticate(state, headers, false).await?;
    let repo =
        codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, repository)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    if let Some(reader) = &auth {
        let bounds = crate::backend::identity::github_apps::authority(&state.pool, reader.token.id)
            .await
            .map_err(Error::unavailable)?;
        if scope == "contents"
            && !crate::backend::identity::oauth_apps::allows_git(
                &state.pool,
                reader.token.id,
                bounds.as_ref(),
                repo.is_gist,
                false,
            )
            .await
            .map_err(Error::unavailable)?
        {
            // An OAuth token without repo scope may still read public content.
            auth = None;
        } else {
            reader
                .repository_scope(&state.pool, repository, scope, false)
                .await
                .map_err(scope_error)?;
        }
    }
    if !crate::backend::repositories::access::can_read(
        &state.pool,
        auth.as_ref().map(|reader| &reader.user),
        &repo,
    )
    .await
    .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok(())
}

/// Axum serves HEAD through GET routes; neither method may request write authority or mutate.
pub(super) fn read_method(method: &Method) -> bool {
    *method == Method::GET || *method == Method::HEAD
}

impl Authentication {
    pub fn credential(&self) -> codeyard_db_core::queries::users::accounts::AccountCredential<'_> {
        codeyard_db_core::queries::users::accounts::AccountCredential::Token(
            self.token.id,
            &self.token.token_hash,
        )
    }

    /// The authenticated token, so database admission re-checks it is still live.
    pub fn database_actor(&self) -> codeyard_db_core::queries::credentials::Actor {
        (self.user.id, &self.credential()).into()
    }

    pub fn ci_credential(
        &self,
        headers: &HeaderMap,
        permission: &'static str,
    ) -> Result<codeyard_db_core::queries::ci::CiCredential> {
        Ok(codeyard_db_core::queries::ci::CiCredential::Token {
            id: self.token.id,
            hash: token_hash(credential(headers).ok_or_else(required)?),
            permission,
        })
    }

    pub fn is_job(&self) -> bool {
        self.user.username == codeyard_db_core::queries::ci_tokens::BOT
    }

    pub async fn repository_scope(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        scope: &str,
        write: bool,
    ) -> std::result::Result<(), ScopeError> {
        // An App or fine-grained credential reaches only its selected repositories with its
        // granted permissions, also when a request reads a second repository, such as a
        // compared fork. Job scopes are spelled with hyphens; App permissions with underscores.
        if let Some(bounds) = crate::backend::identity::github_apps::authority(pool, self.token.id)
            .await
            .map_err(|_| ScopeError::Unavailable)?
        {
            let granted = bounds
                .repository(pool, self.user.id, repo_id)
                .await
                .map_err(|_| ScopeError::Unavailable)?
                .ok_or(ScopeError::Missing)?;
            if !codeyard_model::github_apps::allows(&granted, &scope.replace('-', "_"), write) {
                return Err(ScopeError::Denied);
            }
            return Ok(());
        }
        if !self.is_job() {
            // An organization may refuse classic tokens.
            if codeyard_db_identity::fine_grained_tokens::classic_refused_repository(
                pool,
                self.token.id,
                repo_id,
            )
            .await
            .map_err(|_| ScopeError::Unavailable)?
            {
                return Err(ScopeError::Denied);
            }
            return Ok(());
        }
        let token = codeyard_db_actions::ci_tokens::get(pool, self.token.id)
            .await
            .map_err(|_| ScopeError::Unavailable)?
            .ok_or(ScopeError::Denied)?;
        if token.repo_id != repo_id {
            return Err(ScopeError::Missing);
        }
        let permissions: codeyard_model::permissions::Permissions =
            serde_json::from_str(&token.permissions).map_err(|_| ScopeError::Unavailable)?;
        if !permissions.allows(scope, write) {
            return Err(ScopeError::Denied);
        }
        Ok(())
    }

    /// Dispatches may use a live job token, but host execution retains its original actor's authority.
    pub async fn workflow_actor(&self, state: &AppState, repo: i64) -> Result<i64> {
        if self.user.username != codeyard_db_core::queries::ci_tokens::BOT {
            return Ok(self.user.id);
        }
        let token = codeyard_db_actions::ci_tokens::get(&state.pool, self.token.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(required)?;
        if token.repo_id != repo {
            return Err(Error::missing());
        }
        codeyard_db_actions::ci::get_ci_run_by_id(&state.pool, repo, token.run_id)
            .await
            .map_err(Error::unavailable)?
            .and_then(|run| run.triggered_by_id)
            .ok_or_else(required)
    }

    /// Checks that the token still works, for a request whose body took long to arrive after
    /// it authenticated: a token revoked meanwhile must not publish that body.
    pub async fn require_live(&self, pool: &codeyard_db_core::DbPool) -> Result<()> {
        codeyard_db_identity::users::tokens::token_user(pool, self.token.id, true)
            .await
            .map_err(Error::unavailable)?
            .filter(|user| user.id == self.user.id)
            .map(|_| ())
            .ok_or_else(|| Error::new(StatusCode::UNAUTHORIZED, "Bad credentials"))
    }

    pub fn require_write(&self) -> Result<()> {
        if self.token.scope == "write" {
            Ok(())
        } else {
            Err(Error::new(
                StatusCode::FORBIDDEN,
                "Resource not accessible by token",
            ))
        }
    }
}

/// The repositories among `ids` this request may read: the user's own access, and for a
/// private repository also the credential's bounds (an App's or fine-grained token's
/// selection, a job's repository, an organization that refuses classic tokens). Cross-
/// repository lists and searches call this so they reach no more than direct requests do.
pub(super) async fn readable_repositories(
    state: &AppState,
    auth: Option<&Authentication>,
    ids: &[i64],
    scope: &str,
) -> Result<std::collections::HashSet<i64>> {
    if let Some(auth) = auth {
        if codeyard_db_identity::users::tokens::token_user(&state.pool, auth.token.id, false)
            .await
            .map_err(Error::unavailable)?
            .is_none_or(|user| user.id != auth.user.id)
        {
            return Err(required());
        }
    }
    let mut readable = std::collections::HashSet::new();
    for chunk in ids.chunks(100) {
        let visible = codeyard_db_repos::repositories::access::readable_repository_ids(
            &state.pool,
            auth.map(|auth| auth.user.id),
            chunk,
        )
        .await
        .map_err(Error::unavailable)?;
        let Some(auth) = auth else {
            readable.extend(visible);
            continue;
        };
        for repo in
            codeyard_db_repos::repositories::records::get_repositories_by_ids(&state.pool, &visible)
                .await
                .map_err(Error::unavailable)?
        {
            if !repo.is_private {
                if !codeyard_db_identity::fine_grained_tokens::classic_refused_repository(
                    &state.pool,
                    auth.token.id,
                    repo.id,
                )
                .await
                .map_err(Error::unavailable)?
                {
                    readable.insert(repo.id);
                }
                continue;
            }
            match auth
                .repository_scope(&state.pool, repo.id, scope, false)
                .await
            {
                Ok(()) => {
                    readable.insert(repo.id);
                }
                Err(ScopeError::Unavailable) => return Err(Error::unavailable(())),
                Err(ScopeError::Missing | ScopeError::Denied) => {}
            }
        }
    }
    Ok(readable)
}

/// A linked repository is optional: base-side history does not grant access to a fork.
/// Never revive deleted repositories from an old event's visibility snapshot.
pub(super) async fn referenced_repository(
    state: &AppState,
    auth: Option<&Authentication>,
    id: i64,
) -> Result<Option<codeyard_db_core::models::Repository>> {
    let Some(repo) =
        codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
    else {
        return Ok(None);
    };
    if repo.is_private {
        if let Some(auth) = auth {
            match auth
                .repository_scope(&state.pool, id, "metadata", false)
                .await
            {
                Ok(()) => {}
                Err(ScopeError::Missing | ScopeError::Denied) => return Ok(None),
                Err(error) => return Err(scope_error(error)),
            }
        }
    }
    if crate::backend::repositories::access::can_read(
        &state.pool,
        auth.map(|auth| &auth.user),
        &repo,
    )
    .await
    .map_err(Error::unavailable)?
    {
        Ok(Some(repo))
    } else {
        Ok(None)
    }
}

pub(super) fn required() -> Error {
    Error::new(StatusCode::UNAUTHORIZED, "Requires authentication")
}

pub(in crate::backend::api) fn scope_error(error: ScopeError) -> Error {
    match error {
        ScopeError::Missing => Error::missing(),
        ScopeError::Denied => Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token"),
        ScopeError::Unavailable => Error::unavailable(()),
    }
}

pub(super) fn credential(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.split_once(' ')?;
    if !(scheme.eq_ignore_ascii_case("token") || scheme.eq_ignore_ascii_case("bearer"))
        || token.is_empty()
        || token.len() > 2048
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(token)
}

/// These endpoints authenticate application JWTs or HTTP Basic credentials in their handlers.
pub(super) fn application_credentials(path: &str) -> bool {
    path == "/app"
        || path.starts_with("/app/")
        || path.starts_with("/app-manifests/")
        || matches!(
            path.trim_start_matches('/')
                .split('/')
                .collect::<Vec<_>>()
                .as_slice(),
            ["orgs", _, "installation"]
                | ["users", _, "installation"]
                | ["repos", _, _, "installation"]
                | ["apps", _]
        )
        || crate::backend::api::github::apps::client_credentials(path)
}

/// Run credentials cannot leave their repository or borrow another job's permissions.
pub(super) async fn authorize_request(
    axum::Extension(state): axum::Extension<std::sync::Arc<AppState>>,
    params: Option<axum::extract::Path<std::collections::HashMap<String, String>>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let headers = request.headers().clone();
    let request_path = request.uri().path().to_owned();
    let deleting = request.method() == axum::http::Method::DELETE;
    let write = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    );
    let path = request_path
        .strip_prefix("/api/v3")
        .or_else(|| request_path.strip_prefix("/api/uploads"))
        .unwrap_or(&request_path);
    if application_credentials(path) {
        return next.run(request).await;
    }
    let anonymous = std::sync::atomic::AtomicBool::new(false);
    let anonymous_read = &anonymous;
    // The permission the request needs, sent as GitHub sends it to Apps and fine-grained tokens.
    let accepted = std::sync::OnceLock::new();
    let accepted_permission = &accepted;
    let checked = async move {
        if !headers.contains_key(header::AUTHORIZATION) {
            return Ok(None);
        }
        let Some(auth) = authenticate(&state, &headers, false).await? else {
            return Ok(None);
        };
        let path = request_path
            .strip_prefix("/api/v3")
            .or_else(|| request_path.strip_prefix("/api/uploads"))
            .unwrap_or(&request_path);
        if let Some(bounds) =
            crate::backend::identity::github_apps::authority(&state.pool, auth.token.id)
                .await
                .map_err(Error::unavailable)?
        {
            if let Some(grant) = bounds.personal() {
                let need = crate::backend::identity::fine_grained::need(path, write);
                if let Some(key) = need.permission() {
                    let _ = accepted_permission.set(format!("{key}={}", level(write)));
                }
                if personal(
                    &state,
                    &auth,
                    &bounds,
                    grant,
                    need,
                    params.as_ref(),
                    path,
                    write,
                    deleting,
                )
                .await?
                {
                    anonymous_read.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return Ok(None);
            }
            let special = match path
                .trim_start_matches('/')
                .split('/')
                .collect::<Vec<_>>()
                .as_slice()
            {
                [] | [""] | ["user"] => !write,
                ["installation", "repositories" | "token"] => bounds.installation().is_some(),
                ["user", "installations", ..] => {
                    !write
                        || (bounds.installation().is_none()
                            && codeyard_model::github_apps::allows(
                                &bounds.permissions,
                                "administration",
                                true,
                            ))
                }
                _ => false,
            };
            if special {
                return Ok(None);
            }
            let params = params.as_ref().ok_or_else(Error::missing)?;
            let permission =
                crate::backend::identity::github_apps::repository_permission(path, write)
                    .ok_or_else(|| {
                        Error::new(
                            StatusCode::FORBIDDEN,
                            "Resource not accessible by integration",
                        )
                    })?;
            let _ = accepted_permission.set(format!("{permission}={}", level(write)));
            match repository_bounds(
                &state, &auth, &bounds, params, path, write, deleting, permission,
            )
            .await?
            {
                Some(true) => {}
                None => return Err(Error::missing()),
                Some(false) => {
                    return Err(Error::new(
                        StatusCode::FORBIDDEN,
                        "Resource not accessible by integration",
                    ))
                }
            }
            return Ok(None);
        }
        if let Some(token) = codeyard_db_identity::oauth_apps::token(&state.pool, auth.token.id)
            .await
            .map_err(Error::unavailable)?
        {
            let deleting_repo = deleting
                && matches!(
                    path.trim_start_matches('/')
                        .split('/')
                        .collect::<Vec<_>>()
                        .as_slice(),
                    ["repos", _, _]
                );
            let allowed = if deleting_repo {
                codeyard_model::oauth_apps::has_scope(&token.scopes, "delete_repo")
            } else {
                codeyard_model::oauth_apps::allows(&token.scopes, path, write)
            };
            // Without the repo scope a token still reads public repositories and their
            // networks' events, as an anonymous request does.
            if !allowed && !write && !codeyard_model::oauth_apps::has_scope(&token.scopes, "repo") {
                if let ["repos" | "networks", owner, name, ..] = path
                    .trim_start_matches('/')
                    .split('/')
                    .collect::<Vec<_>>()
                    .as_slice()
                {
                    if codeyard_db_repos::repositories::records::get_repository(
                        &state.pool,
                        owner,
                        name,
                    )
                    .await
                    .map_err(Error::unavailable)?
                    .is_some_and(|repo| !repo.is_private)
                    {
                        anonymous_read.store(true, std::sync::atomic::Ordering::Relaxed);
                        return Ok(Some(token.scopes));
                    }
                }
            }
            let workflow_write = write
                && ((!deleting
                    && matches!(
                        path.trim_start_matches('/')
                            .split('/')
                            .collect::<Vec<_>>()
                            .as_slice(),
                        ["repos", _, _, "git", "refs" | "ref", ..]
                    ))
                    || params
                        .as_ref()
                        .and_then(|params| params.get("path"))
                        .is_some_and(|path| {
                            path == ".github"
                                || path == ".github/workflows"
                                || path.starts_with(".github/workflows/")
                        }));
            if !allowed
                || (workflow_write
                    && !codeyard_model::oauth_apps::has_scope(&token.scopes, "workflow"))
            {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by token",
                ));
            }
            return Ok(Some(token.scopes));
        }
        if auth.user.username != codeyard_db_core::queries::ci_tokens::BOT {
            classic_policy(&state, auth.token.id, params.as_ref(), path).await?;
            return Ok(Some("repo,read:org,gist,workflow".to_owned()));
        }
        if matches!(path, "/user" | "/meta") && !write {
            return Ok(None);
        }
        let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
        if (segments.first() == Some(&"user") && segments.get(1) == Some(&"packages"))
            || (matches!(segments.first().copied(), Some("users" | "orgs"))
                && segments.get(2) == Some(&"packages"))
        {
            // Package transactions enforce the live job's packages permission and linked repository.
            return Ok(None);
        }
        if segments.first() == Some(&"orgs") && segments.get(2) == Some(&"artifacts") {
            // Artifact metadata ingress and transactions check its dedicated permission and exact repository.
            return Ok(None);
        }
        if !write
            && (matches!(
                path,
                "/user"
                    | "/"
                    | "/meta"
                    | "/versions"
                    | "/zen"
                    | "/octocat"
                    | "/rate_limit"
                    | "/licenses"
                    | "/emojis"
                    | "/codes_of_conduct"
                    | "/gitignore/templates"
            ) || path.starts_with("/licenses/")
                || path.starts_with("/emojis/")
                || path.starts_with("/codes_of_conduct/")
                || path.starts_with("/gitignore/templates/"))
        {
            return Ok(None);
        }
        let params = params
            .as_ref()
            .ok_or_else(|| Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token"))?;
        let owner = params.get("owner").ok_or_else(Error::missing)?;
        let name = params.get("repo").ok_or_else(Error::missing)?;
        let repo =
            codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
        let permission = match segments.get(3).copied() {
            Some("code-scanning") if segments.get(4) == Some(&"default-setup") => None,
            Some("code-scanning") => Some("security_events"),
            Some("pulls") => Some("pull_requests"),
            Some("attestations") => Some("attestations"),
            Some("license" | "dispatches") => Some("contents"),
            Some("actions")
                if segments
                    .last()
                    .is_some_and(|part| matches!(*part, "retention-limit" | "storage-limit")) =>
            {
                Some("administration")
            }
            _ => crate::backend::identity::github_apps::repository_permission(path, write),
        }
        .ok_or_else(|| Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token"))?;
        // Jobs share repository route classification, but cannot acquire a credential's
        // administration or workflow capabilities merely by naming those routes.
        if permission == "workflows"
            || permission == "administration" && segments.get(3) != Some(&"actions")
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "Resource not accessible by token",
            ));
        }
        let scope = permission.replace('_', "-");
        match auth
            .repository_scope(&state.pool, repo.id, &scope, write)
            .await
        {
            Ok(()) => {}
            Err(ScopeError::Denied)
                if permission == "issues"
                    && pull_conversation(&state.pool, repo.id, path).await? =>
            {
                auth.repository_scope(&state.pool, repo.id, "pull-requests", write)
                    .await
                    .map_err(scope_error)?;
            }
            Err(error) => return Err(scope_error(error)),
        }
        Ok(None)
    }
    .await;
    if anonymous.into_inner() {
        if let Some(context) = context(request.headers()) {
            context
                .public_only
                .store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            // Keep a safe downgrade for routers that do not install the shared context.
            request.headers_mut().remove(header::AUTHORIZATION);
        }
    }
    let mut response = match checked {
        Ok(scopes) => {
            let mut response = next.run(request).await;
            if let Some(scopes) = scopes {
                response.headers_mut().insert(
                    "x-oauth-scopes",
                    scopes.parse().expect("validated OAuth scopes"),
                );
            }
            response
        }
        Err(error) => error.into_response(),
    };
    if let Some(permission) = accepted.get() {
        response.headers_mut().insert(
            "x-accepted-github-permissions",
            permission.parse().expect("a permission name"),
        );
    }
    response
}

fn level(write: bool) -> &'static str {
    if write {
        "write"
    } else {
        "read"
    }
}

async fn bound_repository(
    state: &AppState,
    params: &std::collections::HashMap<String, String>,
) -> Result<codeyard_db_core::models::Repository> {
    let repo = if let Some(id) = params.get("repository_id") {
        codeyard_db_repos::repositories::records::get_repository_by_id(
            &state.pool,
            id.parse().map_err(|_| Error::missing())?,
        )
        .await
    } else {
        codeyard_db_repos::repositories::records::get_repository(
            &state.pool,
            params.get("owner").ok_or_else(Error::missing)?,
            params.get("repo").ok_or_else(Error::missing)?,
        )
        .await
    };
    repo.map_err(Error::unavailable)?.ok_or_else(Error::missing)
}

/// Numbered collections and individual comment routes address the same PR conversation.
async fn pull_conversation(pool: &codeyard_db_core::DbPool, repo: i64, path: &str) -> Result<bool> {
    let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        ["repos", _, _, "issues", number, "comments"] => match number.parse() {
            Ok(number) => Ok(codeyard_db_repos::pull_requests::records::get_pull_request(
                pool, repo, number,
            )
            .await
            .map_err(Error::unavailable)?
            .is_some()),
            Err(_) => Ok(false),
        },
        ["repos", _, _, "issues", "comments", id, ..] => match id.parse() {
            Ok(id) => Ok(matches!(
                crate::backend::api::github::comments::find(pool, repo, id)
                    .await?
                    .item,
                codeyard_db_repos::assignees::AssignedItem::PullRequest(_)
            )),
            Err(_) => Ok(false),
        },
        _ => Ok(false),
    }
}

/// Checks one repository request against an App's or a fine-grained token's bounds. `None`
/// means the credential cannot reach the repository at all.
#[expect(
    clippy::too_many_arguments,
    reason = "The check takes the credential, path parameters and access kind as the route has them."
)]
async fn repository_bounds(
    state: &AppState,
    auth: &Authentication,
    bounds: &crate::backend::identity::github_apps::Authority,
    params: &std::collections::HashMap<String, String>,
    path: &str,
    write: bool,
    deleting: bool,
    permission: &str,
) -> Result<Option<bool>> {
    use codeyard_model::github_apps::allows;
    let repo = bound_repository(state, params).await?;
    let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    let pull_conversation = pull_conversation(&state.pool, repo.id, path).await?;
    let workflow = write
        && ((!deleting
            && matches!(
                segments.as_slice(),
                ["repos", _, _, "git", "refs" | "ref", ..]
            ))
            || params.get("path").is_some_and(|path| {
                path == ".github"
                    || path == ".github/workflows"
                    || path.starts_with(".github/workflows/")
            }));
    let Some(granted) = bounds
        .repository(&state.pool, auth.user.id, repo.id)
        .await
        .map_err(Error::unavailable)?
    else {
        return Ok(None);
    };
    let allowed = (allows(&granted, permission, write)
        || (pull_conversation && allows(&granted, "pull_requests", write)))
        && (!workflow || allows(&granted, "workflows", true));
    // A refusal must not reveal a private repository its user cannot read.
    if !allowed
        && repo.is_private
        && !crate::backend::repositories::access::can_read(&state.pool, Some(&auth.user), &repo)
            .await
            .map_err(Error::unavailable)?
    {
        return Ok(None);
    }
    Ok(Some(allowed))
}

/// Checks a fine-grained token's request. `Ok(true)` asks to answer it as an anonymous one,
/// which reads public data only.
#[expect(
    clippy::too_many_arguments,
    reason = "The check takes the credential, grant, path parameters and access kind as the route has them."
)]
async fn personal(
    state: &AppState,
    auth: &Authentication,
    bounds: &crate::backend::identity::github_apps::Authority,
    grant: &crate::backend::identity::fine_grained::Grant,
    need: crate::backend::identity::fine_grained::Need,
    params: Option<&axum::extract::Path<std::collections::HashMap<String, String>>>,
    path: &str,
    write: bool,
    deleting: bool,
) -> Result<bool> {
    use crate::backend::identity::fine_grained::Need;
    use crate::backend::identity::fine_grained::Owner;
    use codeyard_model::fine_grained::allows;
    let denied = || {
        Error::new(
            StatusCode::FORBIDDEN,
            "Resource not accessible by personal access token",
        )
    };
    let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    let organization = |name: &str| {
        let name = name.to_owned();
        async move {
            codeyard_db_orgs::organizations::get_public_organization(&state.pool, &name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)
        }
    };
    let allowed = match need {
        Need::Free => true,
        Need::Public => return Ok(true),
        Need::Denied => false,
        Need::Account(key) => grant.account(auth.user.id, key, write),
        Need::Organization(key) => {
            let org = organization(segments.get(1).copied().unwrap_or_default()).await?;
            grant.organization(org.id, key, write)
                && (!(write
                    && key == "organization_custom_properties"
                    && segments.get(3) == Some(&"schema"))
                    || grant
                        .permissions
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        == Some("admin"))
        }
        Need::CreateRepository => {
            let owner = match segments.as_slice() {
                ["user", "repos"] => Some(Owner::User(auth.user.id)),
                ["orgs", name, "repos"] => Some(Owner::Organization(organization(name).await?.id)),
                _ => None,
            };
            owner.is_some_and(|owner| grant.creates_repository(owner))
        }
        Need::CopyRepository(key) => {
            let params = params.ok_or_else(Error::missing)?;
            // The source is only read; a private one outside the token reads as missing.
            repository_bounds(state, auth, bounds, params, path, false, false, key)
                .await?
                .ok_or_else(Error::missing)?
        }
        Need::Packages => {
            let owner = match segments.as_slice() {
                ["user", "packages", ..] => Some(Owner::User(auth.user.id)),
                ["users", name, "packages", ..] => {
                    codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, name)
                        .await
                        .map_err(Error::unavailable)?
                        .map(|user| Owner::User(user.id))
                }
                ["orgs", name, "packages", ..] => {
                    Some(Owner::Organization(organization(name).await?.id))
                }
                _ => None,
            };
            if grant.active
                && owner == Some(grant.owner)
                && allows(&grant.permissions, "packages", write)
            {
                true
            } else if !write {
                // Other owners' public packages read as they do anonymously.
                return Ok(true);
            } else {
                false
            }
        }
        Need::Repository(key) => {
            let params = params.ok_or_else(Error::missing)?;
            // A private repository outside the token reads as missing, as on GitHub.
            let allowed =
                repository_bounds(state, auth, bounds, params, path, write, deleting, key)
                    .await?
                    .ok_or_else(Error::missing)?;
            // A public repository outside the token's selection, or a token not yet approved,
            // reads as an anonymous request does: the user's own rights must not apply.
            if allowed && !write {
                let repo = bound_repository(state, params).await?;
                if !grant
                    .reaches(&state.pool, &repo)
                    .await
                    .map_err(Error::unavailable)?
                {
                    return Ok(true);
                }
            }
            allowed
        }
    };
    if allowed {
        Ok(false)
    } else {
        Err(denied())
    }
}

/// A fine-grained token copies a repository (a fork or a template) only into an owner it may
/// create repositories for. The request body names that owner, so the handler calls this.
/// App credentials cannot copy repositories.
pub(super) async fn require_copy_owner(
    state: &AppState,
    auth: &Authentication,
    target: codeyard_db_repos::repositories::creation::RepositoryTarget,
) -> Result<()> {
    use crate::backend::identity::fine_grained::Owner;
    use codeyard_db_repos::repositories::creation::RepositoryTarget;
    let Some(bounds) = crate::backend::identity::github_apps::authority(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
    else {
        return Ok(());
    };
    let owner = match target {
        RepositoryTarget::Personal(id) => Owner::User(id),
        RepositoryTarget::Organization(id) => Owner::Organization(id),
    };
    match bounds.personal() {
        Some(grant) if grant.creates_repository(owner) => Ok(()),
        Some(_) => Err(Error::new(
            StatusCode::FORBIDDEN,
            "Resource not accessible by personal access token",
        )),
        None => Err(Error::new(
            StatusCode::FORBIDDEN,
            "Resource not accessible by integration",
        )),
    }
}

/// An organization may refuse classic personal tokens for its repositories and its own API.
async fn classic_policy(
    state: &AppState,
    token: i64,
    params: Option<&axum::extract::Path<std::collections::HashMap<String, String>>>,
    path: &str,
) -> Result<()> {
    let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    let org = match segments.as_slice() {
        ["repos", _, _, ..] => {
            let (Some(owner), Some(name)) = (
                params.and_then(|params| params.get("owner")),
                params.and_then(|params| params.get("repo")),
            ) else {
                return Ok(());
            };
            codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, name)
                .await
                .map_err(Error::unavailable)?
                .and_then(|repo| repo.owner_org_id)
        }
        ["repositories", id, ..] => codeyard_db_repos::repositories::records::get_repository_by_id(
            &state.pool,
            id.parse().map_err(|_| Error::missing())?,
        )
        .await
        .map_err(Error::unavailable)?
        .and_then(|repo| repo.owner_org_id),
        ["orgs" | "organizations", name, ..] => {
            codeyard_db_orgs::organizations::get_public_organization(&state.pool, name)
                .await
                .map_err(Error::unavailable)?
                .map(|org| org.id)
        }
        _ => None,
    };
    match org {
        Some(org) => refuse_classic(state, token, org).await,
        None => Ok(()),
    }
}

/// Refuses a classic personal token for organization `org` when it forbids them. Routes that
/// name the organization only through another resource (a team or archive ID) call this once
/// they resolve it.
pub(super) async fn refuse_classic(state: &AppState, token: i64, org: i64) -> Result<()> {
    if codeyard_db_identity::fine_grained_tokens::classic_refused(&state.pool, token, org)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "This organization forbids access with a personal access token (classic). Use a fine-grained personal access token, a GitHub App or an OAuth App.",
        ));
    }
    Ok(())
}

pub(in crate::backend::api) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    required_auth: bool,
) -> Result<Option<Authentication>> {
    let identity = identity(state, headers, required_auth).await?;
    if context(headers).is_some_and(|context| {
        context
            .public_only
            .load(std::sync::atomic::Ordering::Relaxed)
    }) {
        return Ok(None);
    }
    Ok(identity)
}

/// For operations that explicitly apply repository-permission filtering themselves, keep
/// verified identity independent of the default public-data audience.
pub(super) async fn identity(
    state: &AppState,
    headers: &HeaderMap,
    required_auth: bool,
) -> Result<Option<Authentication>> {
    let identity = if let Some(context) = context(headers) {
        context
            .authentication
            .get_or_try_init(|| authenticate_uncached(state, headers))
            .await?
            .clone()
    } else {
        authenticate_uncached(state, headers).await?
    };
    if required_auth && identity.is_none() {
        return Err(required());
    }
    Ok(identity)
}

async fn authenticate_uncached(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<Authentication>> {
    let config = codeyard_db_cluster::node_config::get_node_configuration(&state.pool)
        .await
        .map_err(Error::unavailable)?
        .filter(|config| config.setup_completed)
        .ok_or_else(Error::missing)?;
    if !headers.contains_key(header::AUTHORIZATION) {
        return if config.is_private() {
            Err(required())
        } else {
            Ok(None)
        };
    }
    let presented = credential(headers);
    let token = match presented {
        Some(value) => {
            codeyard_db_identity::users::tokens::find_token(&state.pool, &token_hash(value))
                .await
                .map_err(Error::unavailable)?
        }
        None => None,
    };
    let mut user_id = None;
    if let Some(token) = token {
        if let Some(user) =
            codeyard_db_identity::users::tokens::token_user(&state.pool, token.id, false)
                .await
                .map_err(Error::unavailable)?
        {
            touch(state, &token, headers).await;
            return Ok(Some(Authentication { token, user }));
        }
        user_id = Some(token.user_id);
    }
    match crate::backend::identity::admission::admit_login(
        &state.pool,
        crate::backend::api::http::session::client_address(headers),
        user_id,
        presented.unwrap_or("github-rest"),
    )
    .await
    {
        Ok(_) => Err(Error::new(StatusCode::UNAUTHORIZED, "Bad credentials")),
        Err(crate::backend::identity::admission::AdmissionError::Limited) => Err(Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed logins",
        )),
        Err(crate::backend::identity::admission::AdmissionError::Unavailable) => {
            Err(Error::unavailable(()))
        }
    }
}

/// Records a token's last use at most once a minute, so most requests only read.
pub(in crate::backend::api) async fn touch(
    state: &AppState,
    token: &PersonalAccessToken,
    headers: &HeaderMap,
) {
    if token
        .last_used_at
        .is_some_and(|at| chrono::Utc::now() - at < chrono::Duration::minutes(1))
    {
        return;
    }
    let address = crate::backend::api::http::session::client_address(headers)
        .filter(|address| address.len() <= 64);
    if let Err(error) =
        codeyard_db_identity::users::tokens::touch_token(&state.pool, token.id, address).await
    {
        tracing::warn!(%error, "could not record token use");
    }
}
