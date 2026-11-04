//! Token-only authentication, separate from browser sessions and CSRF.
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;

use codeyard_identity::github_auth::REQUEST_CONTEXT;

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
        let bounds = codeyard_identity::github_apps::authority(&state.pool, reader.token.id)
            .await
            .map_err(Error::unavailable)?;
        if scope == "contents"
            && !codeyard_identity::oauth_apps::allows_git(
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
    if !codeyard_identity::repository_access::can_read(
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
    if codeyard_identity::repository_access::can_read(
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

pub fn scope_error(error: ScopeError) -> Error {
    match error {
        ScopeError::Missing => Error::missing(),
        ScopeError::Denied => Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token"),
        ScopeError::Unavailable => Error::unavailable(()),
    }
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
        || crate::apps::client_credentials(path)
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
        if let Some(bounds) = codeyard_identity::github_apps::authority(&state.pool, auth.token.id)
            .await
            .map_err(Error::unavailable)?
        {
            if let Some(grant) = bounds.personal() {
                let need = codeyard_identity::fine_grained::need(path, write);
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
            let permission = codeyard_identity::github_apps::repository_permission(path, write)
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
            _ => codeyard_identity::github_apps::repository_permission(path, write),
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
                codeyard_webhooks::comment_documents::find(pool, repo, id)
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
    bounds: &codeyard_identity::github_apps::Authority,
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
        && !codeyard_identity::repository_access::can_read(&state.pool, Some(&auth.user), &repo)
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
    bounds: &codeyard_identity::github_apps::Authority,
    grant: &codeyard_identity::fine_grained::Grant,
    need: codeyard_identity::fine_grained::Need,
    params: Option<&axum::extract::Path<std::collections::HashMap<String, String>>>,
    path: &str,
    write: bool,
    deleting: bool,
) -> Result<bool> {
    use codeyard_identity::fine_grained::Need;
    use codeyard_identity::fine_grained::Owner;
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
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, &name)
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
    use codeyard_db_repos::repositories::creation::RepositoryTarget;
    use codeyard_identity::fine_grained::Owner;
    let Some(bounds) = codeyard_identity::github_apps::authority(&state.pool, auth.token.id)
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
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, name)
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

use codeyard_identity::github_auth::Authentication;

use codeyard_identity::github_auth::RequestContext;

use codeyard_identity::github_auth::ScopeError;

use codeyard_identity::github_auth::authenticate;

use codeyard_identity::github_auth::context;
