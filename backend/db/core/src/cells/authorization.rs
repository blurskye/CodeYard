//! Home-cell proofs and bounded, short-lived authorization answers. No personal credentials
//! are inserted into a stub's native credential tables.
pub mod freshness;
pub mod heartbeat;
mod scope;
pub use scope::{ensure_current, inherit, scope};

use crate::{models::User, DbPool};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_support::{cells::Prefix, token_hash::token_hash};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

const MAXIMUM_ANSWER_BYTES: usize = 128 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Proof {
    Token { home: Prefix, hash: String },
    Session { home: Prefix, value: String },
    Ssh { fingerprint: String },
}
impl Proof {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Token { hash, .. } => ensure!(
                codeyard_support::tokens::is_hex_token(hash, 64),
                "Invalid credential hash"
            ),
            Self::Session { home, value } => {
                ensure!(
                    codeyard_support::cells::Secret::cookie(value)?.cell == *home,
                    "Invalid session home"
                );
            }
            Self::Ssh { fingerprint } => {
                codeyard_model::cells::Key::new(codeyard_model::cells::KeyKind::Ssh, fingerprint)?;
            }
        }
        Ok(())
    }
    pub fn hash(&self) -> String {
        match self {
            Self::Token { hash, .. } => hash.clone(),
            Self::Session { value, .. } => token_hash(value),
            Self::Ssh { fingerprint } => token_hash(fingerprint),
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Token { .. } => "token",
            Self::Session { .. } => "session",
            Self::Ssh { .. } => "ssh",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Authority {
    Classic,
    OAuth {
        client_id: String,
        scopes: String,
    },
    FineGrained {
        owner_user: Option<i64>,
        owner_org: Option<i64>,
        repositories: Option<Vec<i64>>,
        permissions: Value,
    },
    App {
        owner_user: Option<i64>,
        owner_org: Option<i64>,
        repositories: Option<Vec<i64>>,
        permissions: Value,
    },
    Job {
        repository: i64,
        permissions: Value,
    },
    Session {
        csrf: String,
    },
    Ssh,
}
impl Authority {
    fn validate(&self) -> Result<()> {
        match self {
            Self::OAuth { scopes, .. } => ensure!(
                scopes.len() <= 4096 && !scopes.chars().any(char::is_control),
                "Invalid OAuth scopes"
            ),
            Self::FineGrained {
                owner_user,
                owner_org,
                repositories,
                permissions,
            }
            | Self::App {
                owner_user,
                owner_org,
                repositories,
                permissions,
            } => {
                ensure!(
                    owner_user.is_some() != owner_org.is_some(),
                    "Invalid credential owner"
                );
                ensure!(
                    repositories
                        .as_ref()
                        .is_none_or(|ids| ids.len() <= 1000 && ids.iter().all(|id| *id > 0)),
                    "Too many selected repositories"
                );
                ensure!(
                    permissions
                        .as_object()
                        .is_some_and(|permissions| permissions.len() <= 100),
                    "Invalid credential permissions"
                );
            }
            Self::Job {
                repository,
                permissions,
            } => ensure!(
                *repository > 0
                    && permissions
                        .as_object()
                        .is_some_and(|permissions| permissions.len() <= 100),
                "Invalid job permissions"
            ),
            Self::Session { csrf } => ensure!(
                codeyard_support::tokens::is_hex_token(csrf, 64),
                "Invalid CSRF token"
            ),
            Self::Classic | Self::Ssh => {}
        }
        Ok(())
    }
    /// Resource authority remains in this cell; these bounds can only narrow it.
    pub fn repository(
        &self,
        user: i64,
        repo: &crate::models::Repository,
        permission: &str,
        write: bool,
    ) -> Result<bool> {
        Ok(match self {
            Self::Classic | Self::Session { .. } | Self::Ssh => true,
            Self::OAuth { scopes, .. } => codeyard_model::oauth_apps::allows(
                scopes,
                &if repo.is_gist {
                    "/gists/gist".to_owned()
                } else {
                    crate::queries::credentials::oauth_repository_path(permission)
                },
                write,
            ),
            Self::FineGrained {
                owner_user,
                owner_org,
                repositories,
                permissions,
            }
            | Self::App {
                owner_user,
                owner_org,
                repositories,
                permissions,
            } => {
                let owns = owner_user.is_some_and(|owner| {
                    (matches!(self, Self::App { .. }) || owner == user)
                        && repo.owner_user_id == Some(owner)
                }) || owner_org.is_some_and(|owner| repo.owner_org_id == Some(owner));
                let selected = repositories
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&repo.id));
                let permission = permission.replace('-', "_");
                let permitted = if matches!(self, Self::FineGrained { .. }) {
                    permissions.as_object().is_some_and(|permissions| {
                        codeyard_model::fine_grained::allows(permissions, &permission, write)
                    })
                } else {
                    codeyard_model::github_apps::allows(permissions, &permission, write)
                };
                owns && selected && permitted
            }
            Self::Job {
                repository,
                permissions,
            } => {
                *repository == repo.id
                    && serde_json::from_value::<codeyard_model::permissions::Permissions>(
                        permissions.clone(),
                    )?
                    .allows(&permission.replace('_', "-"), write)
            }
        })
    }
    pub fn organization(&self, org: i64, permission: &str, write: bool) -> bool {
        match self {
            Self::Classic | Self::Session { .. } | Self::Ssh => true,
            Self::OAuth { scopes, .. } => codeyard_model::oauth_apps::allows(
                scopes,
                &crate::queries::credentials::oauth_organization_path(permission),
                write,
            ),
            Self::FineGrained {
                owner_org,
                permissions,
                ..
            } => {
                *owner_org == Some(org)
                    && permissions.as_object().is_some_and(|permissions| {
                        codeyard_model::fine_grained::allows(permissions, permission, write)
                    })
            }
            Self::App { .. } | Self::Job { .. } => false,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    /// Read with the credential in the home database's snapshot, never stamped on receipt.
    pub heartbeat: DateTime<Utc>,
    pub home: Prefix,
    pub user: User,
    pub credential_id: Option<i64>,
    pub write: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub authority: Authority,
}
impl Answer {
    /// Home-cell metadata is limited to the fields a stub may retain.
    pub fn redact(&mut self) {
        redact_user(&mut self.user);
    }
    fn validate(&self, pool: &DbPool, proof: &Proof) -> Result<()> {
        ensure!(
            self.home != pool.cell_id()
                && self.user.home_cell == i32::from(self.home.get())
                && !self.user.is_stub,
            "Invalid foreign account home"
        );
        validate_public_user(&self.user)?;
        ensure!(
            !self.user.is_banned && !self.user.must_change_password,
            "Foreign account is restricted"
        );
        ensure!(
            self.credential_id.is_none_or(|id| id > 0),
            "Invalid credential ID"
        );
        ensure!(
            match proof {
                Proof::Token { .. } =>
                    self.credential_id.is_some()
                        && !matches!(self.authority, Authority::Session { .. } | Authority::Ssh),
                Proof::Session { .. } =>
                    self.credential_id.is_none()
                        && self.write
                        && self.expires_at.is_some()
                        && matches!(self.authority, Authority::Session { .. }),
                Proof::Ssh { .. } =>
                    self.credential_id.is_some() && matches!(self.authority, Authority::Ssh),
            },
            "Credential kind does not match its answer"
        );
        self.authority.validate()
    }
}
pub fn redact_user(user: &mut User) {
    user.email = None;
    user.password_hash = None;
    user.avatar_file_id = None;
    user.is_admin = false;
    user.email_verified = false;
}
pub fn validate_public_user(user: &User) -> Result<()> {
    ensure!(
        user.email.is_none()
            && user.password_hash.is_none()
            && user.avatar_file_id.is_none()
            && !user.is_admin
            && !user.email_verified,
        "Foreign account includes private metadata"
    );
    Ok(())
}

/// The provider authenticates the answering node and verifies its directory ownership.
/// It never grants resource membership or repository authority.
pub trait Home: Send + Sync {
    fn send_push<'a>(&'a self, _event: &'a super::pulls::Event) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Foreign push delivery is unavailable") })
    }
    fn receive_push<'a>(
        &'a self,
        _event: &'a super::pulls::Event,
        _admission: &'a dyn super::pulls::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Foreign push admission is unavailable") })
    }
    fn send_pull<'a>(
        &'a self,
        _binding: &'a super::pulls::Binding,
        _proof: &'a Proof,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Foreign pull subscription is unavailable") })
    }
    fn receive_pull<'a>(
        &'a self,
        _binding: &'a super::pulls::Binding,
        _proof: &'a Proof,
        _admission: &'a dyn super::pulls::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Foreign pull subscription is unavailable") })
    }
    fn pull_grant<'a>(
        &'a self,
        _binding: &'a super::pulls::Binding,
        _proof: Option<&'a Proof>,
    ) -> BoxFuture<'a, Result<Option<super::pulls::Grant>>> {
        Box::pin(async { anyhow::bail!("Foreign pull authority is unavailable") })
    }
    fn pull_snapshot<'a>(
        &'a self,
        _request: &'a super::pulls::HeadRequest,
        _output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<Option<super::repositories::Snapshot>>> {
        Box::pin(async { anyhow::bail!("Foreign pull snapshots are unavailable") })
    }
    fn forward_fork<'a>(
        &'a self,
        _owner: &'a str,
        _request: &'a super::repositories::ForwardFork,
    ) -> BoxFuture<'a, Result<Option<super::repositories::ForwardedFork>>> {
        Box::pin(async { anyhow::bail!("Foreign fork forwarding is unavailable") })
    }
    fn snapshot<'a>(
        &'a self,
        _request: &'a super::repositories::SnapshotRequest,
        _output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<super::repositories::Snapshot>> {
        Box::pin(async { anyhow::bail!("Fork snapshot exports are unavailable") })
    }
    fn repository_id<'a>(
        &'a self,
        _id: i64,
        _proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<super::repositories::Source>>> {
        Box::pin(async { anyhow::bail!("Remote repository metadata is unavailable") })
    }
    fn repository_fork<'a>(
        &'a self,
        _owner: &'a str,
        _base: i64,
        _proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<super::repositories::Source>>> {
        Box::pin(async { anyhow::bail!("Remote fork discovery is unavailable") })
    }
    fn repository_source<'a>(
        &'a self,
        _owner: &'a str,
        _name: &'a str,
        _proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<super::repositories::Source>>> {
        Box::pin(async { anyhow::bail!("Remote fork sources are unavailable") })
    }
    fn cell(&self) -> Option<Prefix> {
        None
    }
    fn resource_page<'a>(
        &'a self,
        _cell: Prefix,
        _page: &'a super::pointers::Page,
        _credential: Option<&'a str>,
    ) -> BoxFuture<'a, Result<super::pointers::PageResult>> {
        Box::pin(async { anyhow::bail!("Remote resource reads are unavailable") })
    }

    fn notice_public(&self, _repository: i64) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async { anyhow::bail!("Notice visibility is unavailable") })
    }
    fn send_notice<'a>(&'a self, _event: &'a super::notices::Event) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Notice delivery is unavailable") })
    }
    fn receive_notice<'a>(
        &'a self,
        _event: &'a super::notices::Event,
        _admission: &'a dyn super::notices::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Notice admission is unavailable") })
    }
    fn send_pointer<'a>(&'a self, _event: &'a super::pointers::Event) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Pointer delivery is unavailable") })
    }
    fn receive_pointer<'a>(
        &'a self,
        _event: &'a super::pointers::Event,
        _admission: &'a dyn super::pointers::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { anyhow::bail!("Pointer admission is unavailable") })
    }
    fn authorize<'a>(&'a self, proof: &'a Proof) -> BoxFuture<'a, Result<Option<Answer>>>;
    fn user_id<'a>(&'a self, _id: i64) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async { anyhow::bail!("Remote account metadata is unavailable") })
    }
    fn user<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<User>>>;
}
pub type Provider = Arc<dyn Home>;

/// Ask the home on every admission. Only this operation retains the answer; another
/// request must obtain a new snapshot even if it presents the same credential.
pub async fn authorize(pool: &DbPool, proof: &Proof) -> Result<Option<Answer>> {
    proof.validate()?;
    scope::forget(proof.kind(), &proof.hash());
    let Some(provider) = pool.foreign_authorization() else {
        return Ok(None);
    };
    let policy = freshness::policy()?;
    let read = policy
        .start(Utc::now(), std::time::Instant::now())
        .ok_or_else(|| anyhow::anyhow!("Identity freshness is unavailable"))?;
    let answer = tokio::time::timeout(policy.maximum_age(), provider.authorize(proof)).await??;
    let Some(answer) = answer else {
        return Ok(None);
    };
    answer.validate(pool, proof)?;
    let deadline = read
        .prove(
            answer.heartbeat,
            answer.expires_at,
            Utc::now(),
            std::time::Instant::now(),
        )
        .ok_or_else(|| anyhow::anyhow!("Identity snapshot is stale"))?;
    ensure!(
        serde_json::to_vec(&answer)?.len() <= MAXIMUM_ANSWER_BYTES,
        "Foreign account is too large"
    );
    // Metadata may survive a failed admission. It never grants access by itself.
    super::stubs::user(pool, answer.home, &answer.user).await?;
    ensure!(
        deadline.current(Utc::now(), std::time::Instant::now()),
        "Identity proof expired"
    );
    scope::retain(scope::Verified {
        proof: proof.clone(),
        answer: answer.clone(),
        kind: proof.kind(),
        hash: proof.hash(),
        deadline,
        refused: false,
    })?;
    Ok(lookup(
        pool,
        &Query {
            kind: proof.kind(),
            id: answer.credential_id,
            hash: Some(&proof.hash()),
            write: false,
        },
    )
    .await?
    .map(|credential| credential.answer))
}

/// Whether a token issued here belongs to an account that has since moved to another cell.
pub async fn moved_token(pool: &DbPool, hash: &str) -> Result<bool> {
    const MOVED: &str = "SELECT EXISTS(SELECT 1 FROM moved_credentials WHERE token_hash=$1)";
    Ok(match pool {
        DbPool::Postgres(pool) => sqlx::query_scalar(MOVED).bind(hash).fetch_one(pool).await?,
        DbPool::Sqlite(pool) => sqlx::query_scalar(MOVED).bind(hash).fetch_one(pool).await?,
    })
}

/// Called before each native database operation, outside its transaction. An operation
/// that outlived its proof asks the home once more instead of reusing the old answer; the
/// operation's own transaction still checks the new deadline.
pub async fn refresh(pool: &DbPool) {
    if scope::any_expired() {
        ask_again(pool).await;
    }
}
/// Boxed because admission itself runs native operations, which call `refresh`.
fn ask_again(pool: &DbPool) -> BoxFuture<'_, ()> {
    Box::pin(async move {
        for old in scope::take_expired() {
            if !matches!(authorize(pool, &old.proof).await, Ok(Some(_))) {
                scope::restore(old);
            }
        }
    })
}

/// Standalone admission for callers that retain the typed result themselves (Git/SSH).
pub async fn admit(pool: &DbPool, proof: &Proof) -> Result<Option<Credential>> {
    scope(async {
        if authorize(pool, proof).await?.is_none() {
            return Ok(None);
        }
        lookup(
            pool,
            &Query {
                kind: proof.kind(),
                id: None,
                hash: Some(&proof.hash()),
                write: false,
            },
        )
        .await
    })
    .await
}

/// An operation lookup carries the exact credential hash through native transactions.
pub struct Query<'a> {
    pub kind: &'static str,
    pub id: Option<i64>,
    pub hash: Option<&'a str>,
    pub write: bool,
}
pub struct Credential {
    pub answer: Answer,
    pub hash: String,
    pub user: User,
    deadline: freshness::Deadline,
}
impl Credential {
    pub fn current(&self) -> bool {
        self.deadline.current(Utc::now(), std::time::Instant::now())
    }
    pub fn deadline(&self) -> freshness::Deadline {
        self.deadline
    }

    pub fn token(&self) -> Option<crate::models::PersonalAccessToken> {
        if !self.current() {
            return None;
        }
        Some(crate::models::PersonalAccessToken {
            id: self.answer.credential_id?,
            token_hash: self.hash.clone(),
            user_id: self.user.id,
            name: "Foreign credential".into(),
            scope: if self.answer.write { "write" } else { "read" }.into(),
            expires_at: self.answer.expires_at,
            created_at: self.user.created_at,
            last_used_at: None,
            last_used_ip: None,
        })
    }
}
fn credential(user: Option<User>, proof: scope::Verified) -> Result<Option<Credential>> {
    let Some(user) = user else { return Ok(None) };
    // The user predicate can be evaluated before a row lock wait. Check both clocks
    // after that wait, retaining the original deadline rather than renewing it.
    ensure!(proof.current(), "Identity proof expired");
    ensure!(
        user.id == proof.answer.user.id
            && user.is_stub
            && user.home_cell == i32::from(proof.answer.home.get()),
        "Foreign account home changed"
    );
    Ok(Some(Credential {
        answer: proof.answer,
        hash: proof.hash,
        user,
        deadline: proof.deadline,
    }))
}
const CREDENTIAL_USER: &str =
    "SELECT * FROM users WHERE id=$1 AND is_stub AND NOT is_banned AND NOT must_change_password";
pub async fn postgres(
    connection: &mut sqlx::PgConnection,
    query: &Query<'_>,
) -> Result<Option<Credential>> {
    let Some(proof) = scope::find(query) else {
        return Ok(None);
    };
    let user = sqlx::query_as(&format!("{CREDENTIAL_USER} FOR SHARE"))
        .bind(proof.answer.user.id)
        .fetch_optional(connection)
        .await?;
    credential(user, proof)
}
pub async fn sqlite(
    connection: &mut sqlx::SqliteConnection,
    query: &Query<'_>,
) -> Result<Option<Credential>> {
    let Some(proof) = scope::find(query) else {
        return Ok(None);
    };
    let user = sqlx::query_as(CREDENTIAL_USER)
        .bind(proof.answer.user.id)
        .fetch_optional(connection)
        .await?;
    credential(user, proof)
}
pub async fn lookup(pool: &DbPool, query: &Query<'_>) -> Result<Option<Credential>> {
    match pool {
        DbPool::Postgres(pool) => postgres(&mut *pool.acquire().await?, query).await,
        DbPool::Sqlite(pool) => sqlite(&mut *pool.acquire().await?, query).await,
    }
}

/// A session read uses the same exact credential hash as its transaction-time check.
pub async fn session(pool: &DbPool, value: &str) -> Result<Option<Credential>> {
    codeyard_support::cells::Secret::cookie(value)?;
    lookup(
        pool,
        &Query {
            kind: "session",
            id: None,
            hash: Some(&token_hash(value)),
            write: false,
        },
    )
    .await
}

pub struct Permission<'a> {
    pub repository: Option<i64>,
    pub organization: Option<i64>,
    pub name: &'a str,
    pub write: bool,
}
fn permits(
    credential: &Credential,
    repository: Option<&crate::models::Repository>,
    permission: &Permission<'_>,
    classic_allowed: bool,
) -> Result<bool> {
    // Repository and organization policy reads may themselves wait for row locks.
    if !credential.current() || (permission.write && !credential.answer.write) {
        return Ok(false);
    }
    if matches!(credential.answer.authority, Authority::Classic) && !classic_allowed {
        return Ok(false);
    }
    match repository {
        Some(repo) => credential.answer.authority.repository(
            credential.user.id,
            repo,
            permission.name,
            permission.write,
        ),
        None => Ok(permission.organization.is_some_and(|org| {
            credential
                .answer
                .authority
                .organization(org, permission.name, permission.write)
        })),
    }
}
pub async fn permits_postgres(
    connection: &mut sqlx::PgConnection,
    actor: i64,
    query: &Query<'_>,
    permission: &Permission<'_>,
) -> Result<bool> {
    let Some(credential) = postgres(connection, query)
        .await?
        .filter(|credential| credential.user.id == actor)
    else {
        return Ok(false);
    };
    let repository: Option<crate::models::Repository> = match permission.repository {
        Some(id) => {
            sqlx::query_as("SELECT * FROM repositories WHERE id=$1 AND NOT is_stub")
                .bind(id)
                .fetch_optional(&mut *connection)
                .await?
        }
        None => None,
    };
    if permission.repository.is_some() && repository.is_none() {
        return Ok(false);
    }
    let organization = permission
        .organization
        .or_else(|| repository.as_ref().and_then(|repo| repo.owner_org_id));
    let allowed: Option<bool> = sqlx::query_scalar(
        "SELECT classic_allowed FROM organization_token_policies WHERE org_id=$1 FOR SHARE",
    )
    .bind(organization)
    .fetch_optional(&mut *connection)
    .await?;
    permits(
        &credential,
        repository.as_ref(),
        permission,
        allowed.unwrap_or(true),
    )
}
pub async fn permits_sqlite(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    query: &Query<'_>,
    permission: &Permission<'_>,
) -> Result<bool> {
    let Some(credential) = sqlite(connection, query)
        .await?
        .filter(|credential| credential.user.id == actor)
    else {
        return Ok(false);
    };
    let repository: Option<crate::models::Repository> = match permission.repository {
        Some(id) => {
            sqlx::query_as("SELECT * FROM repositories WHERE id=$1 AND NOT is_stub")
                .bind(id)
                .fetch_optional(&mut *connection)
                .await?
        }
        None => None,
    };
    if permission.repository.is_some() && repository.is_none() {
        return Ok(false);
    }
    let organization = permission
        .organization
        .or_else(|| repository.as_ref().and_then(|repo| repo.owner_org_id));
    let allowed: Option<bool> = sqlx::query_scalar(
        "SELECT classic_allowed FROM organization_token_policies WHERE org_id=$1",
    )
    .bind(organization)
    .fetch_optional(&mut *connection)
    .await?;
    permits(
        &credential,
        repository.as_ref(),
        permission,
        allowed.unwrap_or(true),
    )
}

/// Only operation-owned session identities; the package transaction still locks and
/// rechecks the account through `session` before using it.
pub fn session_relation(postgres: bool) -> String {
    scope::relation(postgres, "session")
}

/// SQL projections of operation-owned bounds, for queries that retain their credential
/// and read their resource rows in one statement. Aliases `r` and `u` are native rows.
pub fn repository_condition(
    postgres: bool,
    permission: &str,
    write: bool,
    token: &str,
    hash: &str,
) -> String {
    if permission.is_empty()
        || permission.len() > 100
        || !permission
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte == b'-')
    {
        return "FALSE".into();
    }
    let field = |path: &str| {
        if postgres {
            format!("((fc.answer)::jsonb#>>'{{{}}}')", path.replace('.', ","))
        } else {
            format!("json_extract(fc.answer,'$.{path}')")
        }
    };
    let kind = field("authority.kind");
    let enabled = if write {
        format!("CAST({} AS TEXT) IN ('true','1')", field("write"))
    } else {
        "TRUE".into()
    };
    let now = if postgres {
        "clock_timestamp()"
    } else {
        "strftime('%Y-%m-%dT%H:%M:%f000Z','now')"
    };
    let owner=format!("((CAST({} AS BIGINT)=r.owner_user_id AND ({kind}='app' OR r.owner_user_id=u.id)) OR CAST({} AS BIGINT)=r.owner_org_id)",field("authority.owner_user"),field("authority.owner_org"));
    let selected = if postgres {
        "((fc.answer)::jsonb#>'{authority,repositories}'='null'::jsonb OR EXISTS(SELECT 1 FROM jsonb_array_elements_text(COALESCE(NULLIF((fc.answer)::jsonb#>'{authority,repositories}','null'::jsonb),'[]'::jsonb)) selected WHERE CAST(selected.value AS BIGINT)=r.id))".into()
    } else {
        "(json_type(fc.answer,'$.authority.repositories')='null' OR EXISTS(SELECT 1 FROM json_each(json_extract(fc.answer,'$.authority.repositories')) selected WHERE selected.value=r.id))".to_owned()
    };
    let level = field(&format!(
        "authority.permissions.{}",
        permission.replace('-', "_")
    ));
    let levels = if write {
        "'write','admin'"
    } else {
        "'read','write','admin'"
    };
    let fine = format!("{owner} AND {selected} AND {level} IN ({levels})");
    let job_level = field(&format!(
        "authority.permissions.{}",
        permission.replace('_', "-")
    ));
    let job = format!(
        "CAST({} AS BIGINT)=r.id AND CAST({job_level} AS INTEGER)>={}",
        field("authority.repository"),
        if write { 2 } else { 1 }
    );
    let scopes = field("authority.scopes");
    let oauth: Vec<_> = ["repo", "security_events"]
        .into_iter()
        .filter(|scope| {
            codeyard_model::oauth_apps::allows(
                scope,
                &crate::queries::credentials::oauth_repository_path(permission),
                write,
            )
        })
        .map(|scope| format!("(',' || {scopes} || ',') LIKE '%,{scope},%'"))
        .collect();
    let oauth = if oauth.is_empty() {
        "FALSE".into()
    } else {
        oauth.join(" OR ")
    };
    let classic="NOT EXISTS(SELECT 1 FROM organization_token_policies p WHERE p.org_id=r.owner_org_id AND NOT p.classic_allowed)";
    let proofs = scope::relation(postgres, "token");
    format!("EXISTS(SELECT 1 FROM {proofs} fc WHERE fc.kind='token' AND fc.credential_id=CAST({token} AS BIGINT) AND fc.credential_hash={hash} AND fc.user_id=u.id AND u.is_stub AND NOT u.is_banned AND NOT u.must_change_password AND fc.expires_at>{now} AND {enabled} AND CASE {kind} WHEN 'classic' THEN {classic} WHEN 'oauth' THEN ({oauth}) WHEN 'fine_grained' THEN ({fine}) WHEN 'app' THEN ({fine}) WHEN 'job' THEN ({job}) ELSE FALSE END)")
}
