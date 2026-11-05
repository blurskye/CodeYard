//! Token-only authentication, separate from browser sessions and CSRF.
use crate::database_context::DatabaseContext;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_core::models::User;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_support::token_hash::token_hash;
#[derive(Clone)]
pub struct Authentication {
    pub token: PersonalAccessToken,
    pub user: User,
}

tokio::task_local! {
    pub static REQUEST_CONTEXT: std::sync::Arc<RequestContext>;
}

pub struct RequestContext {
    pub authorization: Vec<axum::http::HeaderValue>,
    pub authentication: tokio::sync::OnceCell<Option<Authentication>>,
    pub public_only: std::sync::atomic::AtomicBool,
}

pub fn context(headers: &HeaderMap) -> Option<std::sync::Arc<RequestContext>> {
    let context = REQUEST_CONTEXT.try_with(std::sync::Arc::clone).ok()?;
    context
        .authorization
        .iter()
        .eq(headers.get_all(header::AUTHORIZATION).iter())
        .then_some(context)
}

#[derive(Debug)]
pub enum ScopeError {
    Missing,
    Denied,
    Unavailable,
}

impl Authentication {
    pub fn credential(&self) -> codeyard_db_core::queries::users::accounts::AccountCredential<'_> {
        if self.user.is_stub {
            codeyard_db_core::queries::users::accounts::AccountCredential::ForeignToken(
                self.token.id,
                &self.token.token_hash,
            )
        } else {
            codeyard_db_core::queries::users::accounts::AccountCredential::Token(
                self.token.id,
                &self.token.token_hash,
            )
        }
    }

    pub fn owned_credential(
        &self,
    ) -> codeyard_db_core::queries::users::accounts::OwnedAccountCredential {
        (&self.credential()).into()
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
        if self.user.is_stub {
            return Ok(codeyard_db_core::queries::ci::CiCredential::ForeignToken {
                id: self.token.id,
                hash: token_hash(credential(headers).ok_or_else(required)?),
                permission,
            });
        }
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
        if self.user.is_stub {
            let credential = codeyard_db_core::cells::authorization::lookup(
                pool,
                &codeyard_db_core::cells::authorization::Query {
                    kind: "token",
                    id: Some(self.token.id),
                    hash: Some(&self.token.token_hash),
                    write,
                },
            )
            .await
            .map_err(|_| ScopeError::Unavailable)?
            .ok_or(ScopeError::Denied)?;
            let repo =
                codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo_id)
                    .await
                    .map_err(|_| ScopeError::Unavailable)?
                    .ok_or(ScopeError::Missing)?;
            if matches!(
                credential.answer.authority,
                codeyard_db_core::cells::authorization::Authority::Classic
            ) && repo.owner_org_id.is_some()
            {
                let policy = codeyard_db_identity::fine_grained_tokens::policy(
                    pool,
                    repo.owner_org_id.ok_or(ScopeError::Missing)?,
                )
                .await
                .map_err(|_| ScopeError::Unavailable)?;
                if !policy.classic_allowed {
                    return Err(ScopeError::Denied);
                }
            }
            return if credential
                .answer
                .authority
                .repository(self.user.id, &repo, scope, write)
                .map_err(|_| ScopeError::Unavailable)?
            {
                Ok(())
            } else {
                Err(ScopeError::Denied)
            };
        }
        // An App or fine-grained credential reaches only its selected repositories with its
        // granted permissions, also when a request reads a second repository, such as a
        // compared fork. Job scopes are spelled with hyphens; App permissions with underscores.
        if let Some(bounds) = crate::github_apps::authority(pool, self.token.id)
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
    pub async fn workflow_actor(&self, state: &dyn DatabaseContext, repo: i64) -> Result<i64> {
        if self.user.username != codeyard_db_core::queries::ci_tokens::BOT {
            return Ok(self.user.id);
        }
        let token = codeyard_db_actions::ci_tokens::get(state.pool(), self.token.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(required)?;
        if token.repo_id != repo {
            return Err(Error::missing());
        }
        codeyard_db_actions::ci::get_ci_run_by_id(state.pool(), repo, token.run_id)
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
pub async fn readable_repositories(
    state: &dyn DatabaseContext,
    auth: Option<&Authentication>,
    ids: &[i64],
    scope: &str,
) -> Result<std::collections::HashSet<i64>> {
    if let Some(auth) = auth {
        if codeyard_db_identity::users::tokens::token_user(state.pool(), auth.token.id, false)
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
            state.pool(),
            auth.map(|auth| auth.user.id),
            chunk,
        )
        .await
        .map_err(Error::unavailable)?;
        let Some(auth) = auth else {
            readable.extend(visible);
            continue;
        };
        for repo in codeyard_db_repos::repositories::records::get_repositories_by_ids(
            state.pool(),
            &visible,
        )
        .await
        .map_err(Error::unavailable)?
        {
            if !repo.is_private {
                if !codeyard_db_identity::fine_grained_tokens::classic_refused_repository(
                    state.pool(),
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
                .repository_scope(state.pool(), repo.id, scope, false)
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

pub fn required() -> Error {
    Error::new(StatusCode::UNAUTHORIZED, "Requires authentication")
}

pub use codeyard_support::http_credentials::token as credential;

pub async fn authenticate(
    state: &dyn DatabaseContext,
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
pub async fn identity(
    state: &dyn DatabaseContext,
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
    state: &dyn DatabaseContext,
    headers: &HeaderMap,
) -> Result<Option<Authentication>> {
    let config = codeyard_db_cluster::node_config::get_node_configuration(state.pool())
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
            codeyard_db_identity::users::tokens::find_token(state.pool(), &token_hash(value))
                .await
                .map_err(Error::unavailable)?
        }
        None => None,
    };
    let mut user_id = None;
    if let Some(token) = token {
        if let Some(user) =
            codeyard_db_identity::users::tokens::token_user(state.pool(), token.id, false)
                .await
                .map_err(Error::unavailable)?
        {
            touch(state, &token, headers).await;
            return Ok(Some(Authentication { token, user }));
        }
        user_id = Some(token.user_id);
    }
    match codeyard_db_identity::auth_admission::admit_login(
        state.pool(),
        crate::client_address::client_address(headers),
        user_id,
        presented.unwrap_or("github-rest"),
    )
    .await
    {
        Ok(_) => Err(Error::new(StatusCode::UNAUTHORIZED, "Bad credentials")),
        Err(codeyard_db_identity::auth_admission::AdmissionError::Limited) => Err(Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed logins",
        )),
        Err(codeyard_db_identity::auth_admission::AdmissionError::Unavailable) => {
            Err(Error::unavailable(()))
        }
    }
}

/// Records a token's last use at most once a minute, so most requests only read.
pub async fn touch(state: &dyn DatabaseContext, token: &PersonalAccessToken, headers: &HeaderMap) {
    if token
        .last_used_at
        .is_some_and(|at| chrono::Utc::now() - at < chrono::Duration::minutes(1))
    {
        return;
    }
    let address =
        crate::client_address::client_address(headers).filter(|address| address.len() <= 64);
    if let Err(error) =
        codeyard_db_identity::users::tokens::touch_token(state.pool(), token.id, address).await
    {
        tracing::warn!(%error, "could not record token use");
    }
}
