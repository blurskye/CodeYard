//! Token-only authentication, separate from browser sessions and CSRF.
use super::{Error, Result};
use crate::backend::{
    api::http::session,
    app::AppState,
    db::{
        self,
        models::{PersonalAccessToken, User},
    },
    identity::{admission, tokens::token_hash},
};
use axum::http::{header, HeaderMap, Method, StatusCode};

pub(in crate::backend::api) struct Authentication {
    pub token: PersonalAccessToken,
    pub user: User,
}

#[derive(Debug)]
pub(in crate::backend::api) enum ScopeError {
    Missing,
    Denied,
    Unavailable,
}

/// Axum serves HEAD through GET routes; neither method may request write authority or mutate.
pub(super) fn read_method(method: &Method) -> bool {
    *method == Method::GET || *method == Method::HEAD
}

impl Authentication {
    pub fn is_job(&self) -> bool {
        self.user.username == db::ci_tokens::BOT
    }

    pub async fn repository_scope(
        &self,
        pool: &db::DbPool,
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
            if !crate::backend::identity::github_apps::allows(
                &granted,
                &scope.replace('-', "_"),
                write,
            ) {
                return Err(ScopeError::Denied);
            }
            return Ok(());
        }
        if !self.is_job() {
            // An organization may refuse classic tokens.
            if db::fine_grained_tokens::classic_refused_repository(pool, self.token.id, repo_id)
                .await
                .map_err(|_| ScopeError::Unavailable)?
            {
                return Err(ScopeError::Denied);
            }
            return Ok(());
        }
        let token = db::ci_tokens::get(pool, self.token.id)
            .await
            .map_err(|_| ScopeError::Unavailable)?
            .ok_or(ScopeError::Denied)?;
        if token.repo_id != repo_id {
            return Err(ScopeError::Missing);
        }
        let permissions: crate::backend::ci::tokens::Permissions =
            serde_json::from_str(&token.permissions).map_err(|_| ScopeError::Unavailable)?;
        if !permissions.allows(scope, write) {
            return Err(ScopeError::Denied);
        }
        Ok(())
    }

    /// Dispatches may use a live job token, but host execution retains its original actor's authority.
    pub async fn workflow_actor(&self, state: &AppState, repo: i64) -> Result<i64> {
        if self.user.username != db::ci_tokens::BOT {
            return Ok(self.user.id);
        }
        let token = db::ci_tokens::get(&state.pool, self.token.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(required)?;
        if token.repo_id != repo {
            return Err(Error::missing());
        }
        db::ci::get_ci_run_by_id(&state.pool, repo, token.run_id)
            .await
            .map_err(Error::unavailable)?
            .and_then(|run| run.triggered_by_id)
            .ok_or_else(required)
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

pub(super) fn required() -> Error {
    Error::new(StatusCode::UNAUTHORIZED, "Requires authentication")
}

pub(super) fn scope_error(error: ScopeError) -> Error {
    match error {
        ScopeError::Missing => Error::missing(),
        ScopeError::Denied => Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token"),
        ScopeError::Unavailable => Error::unavailable(()),
    }
}

/// Whether the credential may publish refs that queue workflows: an App needs the workflows
/// permission and an OAuth token the workflow scope. Other tokens carry the user's own rights.
pub(super) async fn workflow_grant(state: &AppState, auth: &Authentication) -> Result<bool> {
    if let Some(bounds) =
        crate::backend::identity::github_apps::authority(&state.pool, auth.token.id)
            .await
            .map_err(Error::unavailable)?
    {
        return Ok(crate::backend::identity::github_apps::allows(
            &bounds.permissions,
            "workflows",
            true,
        ));
    }
    Ok(db::oauth_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
        .is_none_or(|token| {
            crate::backend::identity::oauth_apps::has_scope(&token.scopes, "workflow")
        }))
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
        || super::apps::client_credentials(path)
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
            use crate::backend::identity::github_apps;
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
                            && github_apps::allows(&bounds.permissions, "administration", true))
                }
                _ => false,
            };
            if special {
                return Ok(None);
            }
            let params = params.as_ref().ok_or_else(Error::missing)?;
            let permission = github_apps::repository_permission(path, write).ok_or_else(|| {
                Error::new(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by integration",
                )
            })?;
            let _ = accepted_permission.set(format!("{permission}={}", level(write)));
            if repository_bounds(&state, &auth, &bounds, params, path, write, permission).await?
                != Some(true)
            {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by integration",
                ));
            }
            return Ok(None);
        }
        if let Some(token) = db::oauth_apps::token(&state.pool, auth.token.id)
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
                crate::backend::identity::oauth_apps::has_scope(&token.scopes, "delete_repo")
            } else {
                crate::backend::identity::oauth_apps::allows(&token.scopes, path, write)
            };
            // Without the repo scope a token still reads public repositories, as an anonymous
            // request does.
            if !allowed
                && !write
                && !crate::backend::identity::oauth_apps::has_scope(&token.scopes, "repo")
            {
                if let ["repos", owner, name, ..] = path
                    .trim_start_matches('/')
                    .split('/')
                    .collect::<Vec<_>>()
                    .as_slice()
                {
                    if db::repositories::records::get_repository(&state.pool, owner, name)
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
                    && !crate::backend::identity::oauth_apps::has_scope(&token.scopes, "workflow"))
            {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by token",
                ));
            }
            return Ok(Some(token.scopes));
        }
        if auth.user.username != db::ci_tokens::BOT {
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
        let repo = db::repositories::records::get_repository(&state.pool, owner, name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
        let scope = match segments.get(3).copied() {
            // A job's security-events grant does not include repository administration.
            Some("code-scanning") if segments.get(4) == Some(&"default-setup") => {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by token",
                ));
            }
            None if !write => "metadata",
            Some("statuses") => "statuses",
            Some("dispatches") => "contents",
            // Only /commits/{sha}/<endpoint> selects a status or check permission. Other
            // commit paths, including branch or repository names, need contents.
            Some("commits")
                if segments.len() == 6 && matches!(segments[5], "status" | "statuses") =>
            {
                "statuses"
            }
            Some("commits")
                if segments.len() == 6 && matches!(segments[5], "check-runs" | "check-suites") =>
            {
                "checks"
            }
            Some(
                "contents" | "git" | "commits" | "compare" | "releases" | "tags" | "branches"
                | "zipball" | "tarball" | "readme" | "license",
            ) => "contents",
            Some("issues" | "labels" | "milestones" | "assignees") => "issues",
            Some("pulls") => "pull-requests",
            Some("actions")
                if segments
                    .last()
                    .is_some_and(|part| matches!(*part, "retention-limit" | "storage-limit")) =>
            {
                "administration"
            }
            Some("actions") => "actions",
            Some("attestations") => "attestations",
            Some("check-runs" | "check-suites") => "checks",
            Some("deployments") => "deployments",
            Some("code-scanning") => "security-events",
            Some("pages") => "pages",
            _ => {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by token",
                ))
            }
        };
        auth.repository_scope(&state.pool, repo.id, scope, write)
            .await
            .map_err(scope_error)?;
        Ok(None)
    }
    .await;
    if anonymous.into_inner() {
        request.headers_mut().remove(header::AUTHORIZATION);
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

/// Checks one repository request against an App's or a fine-grained token's bounds. `None`
/// means the credential cannot reach the repository at all.
async fn repository_bounds(
    state: &AppState,
    auth: &Authentication,
    bounds: &crate::backend::identity::github_apps::Authority,
    params: &std::collections::HashMap<String, String>,
    path: &str,
    write: bool,
    permission: &str,
) -> Result<Option<bool>> {
    use crate::backend::identity::github_apps::allows;
    let owner = params.get("owner").ok_or_else(Error::missing)?;
    let name = params.get("repo").ok_or_else(Error::missing)?;
    let repo = db::repositories::records::get_repository(&state.pool, owner, name)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    // A pull request's conversation is its issue's, so its comments also accept the pull
    // request grant.
    let pull_conversation = match segments.as_slice() {
        ["repos", _, _, "issues", number, "comments"] => match number.parse() {
            Ok(number) => {
                db::pull_requests::records::get_pull_request(&state.pool, repo.id, number)
                    .await
                    .map_err(Error::unavailable)?
                    .is_some()
            }
            Err(_) => false,
        },
        _ => false,
    };
    let workflow = write
        && (matches!(
            segments.as_slice(),
            ["repos", _, _, "git", "refs" | "ref", ..]
        ) || params.get("path").is_some_and(|path| {
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
#[allow(clippy::too_many_arguments)]
async fn personal(
    state: &AppState,
    auth: &Authentication,
    bounds: &crate::backend::identity::github_apps::Authority,
    grant: &crate::backend::identity::fine_grained::Grant,
    need: crate::backend::identity::fine_grained::Need,
    params: Option<&axum::extract::Path<std::collections::HashMap<String, String>>>,
    path: &str,
    write: bool,
) -> Result<bool> {
    use crate::backend::identity::fine_grained::{allows, Access, Need, Owner};
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
            db::organizations::get_public_organization(&state.pool, &name)
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
        }
        Need::CreateRepository => {
            let owner = match segments.as_slice() {
                ["user", "repos"] => Some(Owner::User(auth.user.id)),
                ["orgs", name, "repos"] => Some(Owner::Organization(organization(name).await?.id)),
                _ => None,
            };
            grant.active
                && grant.access == Access::All
                && owner == Some(grant.owner)
                && allows(&grant.permissions, "administration", true)
        }
        Need::Packages => {
            let owner = match segments.as_slice() {
                ["user", "packages", ..] => Some(Owner::User(auth.user.id)),
                ["users", name, "packages", ..] => {
                    db::users::accounts::get_user_by_username(&state.pool, name)
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
            repository_bounds(state, auth, bounds, params, path, write, key)
                .await?
                .ok_or_else(Error::missing)?
        }
    };
    if allowed {
        Ok(false)
    } else {
        Err(denied())
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
            db::repositories::records::get_repository(&state.pool, owner, name)
                .await
                .map_err(Error::unavailable)?
                .and_then(|repo| repo.owner_org_id)
        }
        ["orgs", name, ..] => db::organizations::get_public_organization(&state.pool, name)
            .await
            .map_err(Error::unavailable)?
            .map(|org| org.id),
        _ => None,
    };
    if let Some(org) = org {
        if db::fine_grained_tokens::classic_refused(&state.pool, token, org)
            .await
            .map_err(Error::unavailable)?
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "This organization forbids access with a personal access token (classic). Use a fine-grained personal access token, a GitHub App or an OAuth App.",
            ));
        }
    }
    Ok(())
}

pub(in crate::backend::api) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    required_auth: bool,
) -> Result<Option<Authentication>> {
    let config = db::node_config::get_node_configuration(&state.pool)
        .await
        .map_err(Error::unavailable)?
        .filter(|config| config.setup_completed)
        .ok_or_else(Error::missing)?;
    if !headers.contains_key(header::AUTHORIZATION) {
        return if required_auth || config.is_private() {
            Err(required())
        } else {
            Ok(None)
        };
    }
    let presented = credential(headers);
    let token = match presented {
        Some(value) => db::users::tokens::find_token(&state.pool, &token_hash(value))
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    let mut user_id = None;
    if let Some(token) = token {
        if let Some(user) = db::users::tokens::token_user(&state.pool, token.id, false)
            .await
            .map_err(Error::unavailable)?
        {
            touch(state, &token, headers).await;
            return Ok(Some(Authentication { token, user }));
        }
        user_id = Some(token.user_id);
    }
    match admission::admit_login(
        &state.pool,
        session::client_address(headers),
        user_id,
        presented.unwrap_or("github-rest"),
    )
    .await
    {
        Ok(_) => Err(Error::new(StatusCode::UNAUTHORIZED, "Bad credentials")),
        Err(admission::AdmissionError::Limited) => Err(Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed logins",
        )),
        Err(admission::AdmissionError::Unavailable) => Err(Error::unavailable(())),
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
    let address = session::client_address(headers).filter(|address| address.len() <= 64);
    if let Err(error) = db::users::tokens::touch_token(&state.pool, token.id, address).await {
        tracing::warn!(%error, "could not record token use");
    }
}
