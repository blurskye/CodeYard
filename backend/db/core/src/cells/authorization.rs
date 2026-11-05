//! Home-cell proofs and bounded, short-lived authorization answers. No personal credentials
//! are inserted into a stub's native credential tables.
use crate::{models::User, DbPool};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_support::{cells::Prefix, token_hash::token_hash};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub const CACHE_SECONDS: i64 = 59;
const CACHE_SLOTS: u32 = 10000;
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
            Self::Session { value, .. } => {
                codeyard_support::cells::Secret::cookie(value)?;
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
    fn key(&self) -> Result<String> {
        Ok(token_hash(&serde_json::to_string(self)?))
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
            Self::OAuth { scopes } => ensure!(
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
                !csrf.is_empty() && csrf.len() <= 128 && !csrf.chars().any(char::is_control),
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
            Self::OAuth { scopes } => codeyard_model::oauth_apps::allows(
                scopes,
                &crate::queries::credentials::oauth_repository_path(permission),
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
            Self::OAuth { scopes } => codeyard_model::oauth_apps::allows(
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
        self.user.email = None;
        self.user.password_hash = None;
        self.user.avatar_file_id = None;
        self.user.is_admin = false;
        self.user.email_verified = false;
    }
    fn validate(&self, pool: &DbPool, proof: &Proof) -> Result<()> {
        ensure!(
            self.home != pool.cell_id()
                && self.user.home_cell == i32::from(self.home.get())
                && !self.user.is_stub,
            "Invalid foreign account home"
        );
        ensure!(
            self.user.email.is_none()
                && self.user.password_hash.is_none()
                && self.user.avatar_file_id.is_none()
                && !self.user.is_admin
                && !self.user.email_verified,
            "Foreign account includes private metadata"
        );
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
                        && matches!(self.authority, Authority::Session { .. }),
                Proof::Ssh { .. } =>
                    self.credential_id.is_some() && matches!(self.authority, Authority::Ssh),
            },
            "Credential kind does not match its answer"
        );
        self.authority.validate()
    }
}
/// The provider authenticates the answering node and verifies its directory ownership.
/// It never grants resource membership or repository authority.
pub trait Home: Send + Sync {
    fn authorize<'a>(&'a self, proof: &'a Proof) -> BoxFuture<'a, Result<Option<Answer>>>;
}
pub type Provider = Arc<dyn Home>;

#[derive(sqlx::FromRow)]
struct Cached {
    answer: String,
    user_id: i64,
    home_cell: i32,
}
const CURRENT_POSTGRES: &str = "expires_at>clock_timestamp()";
const CURRENT_SQLITE: &str = "expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')";
async fn cached(pool: &DbPool, key: &str) -> Result<Option<Answer>> {
    let row: Option<Cached> = match pool {
        DbPool::Postgres(pool)=>sqlx::query_as(&format!("SELECT f.answer,f.user_id,u.home_cell FROM foreign_credentials f JOIN users u ON u.id=f.user_id WHERE f.cache_key=$1 AND u.is_stub AND NOT u.is_banned AND NOT u.must_change_password AND f.{CURRENT_POSTGRES}" )).bind(key).fetch_optional(pool).await?,
        DbPool::Sqlite(pool)=>sqlx::query_as(&format!("SELECT f.answer,f.user_id,u.home_cell FROM foreign_credentials f JOIN users u ON u.id=f.user_id WHERE f.cache_key=$1 AND u.is_stub AND NOT u.is_banned AND NOT u.must_change_password AND f.{CURRENT_SQLITE}" )).bind(key).fetch_optional(pool).await?,
    };
    row.map(|row| {
        ensure!(
            row.answer.len() <= MAXIMUM_ANSWER_BYTES,
            "Cached account is too large"
        );
        let answer: Answer = serde_json::from_str(&row.answer)?;
        ensure!(
            answer.user.id == row.user_id && i32::from(answer.home.get()) == row.home_cell,
            "Cached account home changed"
        );
        Ok(answer)
    })
    .transpose()
}
/// The cache deadline starts before the RPC, so a slow response cannot extend revocation.
/// A fixed slot count bounds memory on disk and contention is confined to that slot.
pub async fn authorize(pool: &DbPool, proof: &Proof) -> Result<Option<Answer>> {
    proof.validate()?;
    let key = proof.key()?;
    if let Some(answer) = cached(pool, &key).await? {
        return Ok(Some(answer));
    }
    let Some(provider) = pool.foreign_authorization() else {
        return Ok(None);
    };
    let now: DateTime<Utc> = match pool {
        DbPool::Postgres(pool) => {
            sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(pool)
                .await?
        }
        DbPool::Sqlite(pool) => {
            sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
                .fetch_one(pool)
                .await?
        }
    };
    let answer = tokio::time::timeout(Duration::from_secs(10), provider.authorize(proof)).await??;
    let Some(answer) = answer else {
        return Ok(None);
    };
    answer.validate(pool, proof)?;
    let deadline = (now + chrono::Duration::seconds(CACHE_SECONDS))
        .min(answer.expires_at.unwrap_or(DateTime::<Utc>::MAX_UTC));
    let serialized = serde_json::to_string(&answer)?;
    ensure!(
        serialized.len() <= MAXIMUM_ANSWER_BYTES,
        "Foreign account is too large"
    );
    // A failed credential-cache insert may retain harmless metadata; no local credential is created.
    super::stubs::user(pool, answer.home, &answer.user).await?;
    let slot = u32::from_str_radix(&key[..8], 16)? % CACHE_SLOTS;
    const INSERT:&str="INSERT INTO foreign_credentials(slot,cache_key,kind,credential_id,user_id,answer,validated_at,expires_at,credential_hash) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9 FROM users WHERE id=$5 AND is_stub AND home_cell=$10 AND NOT is_banned AND NOT must_change_password ON CONFLICT(slot) DO UPDATE SET cache_key=excluded.cache_key,kind=excluded.kind,credential_id=excluded.credential_id,credential_hash=excluded.credential_hash,user_id=excluded.user_id,answer=excluded.answer,validated_at=excluded.validated_at,expires_at=excluded.expires_at WHERE foreign_credentials.validated_at<=excluded.validated_at";
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query(INSERT)
                .bind(i64::from(slot))
                .bind(&key)
                .bind(proof.kind())
                .bind(answer.credential_id)
                .bind(answer.user.id)
                .bind(serialized)
                .bind(now)
                .bind(deadline)
                .bind(proof.hash())
                .bind(i32::from(answer.home.get()))
                .execute(pool)
                .await?;
        }
        DbPool::Sqlite(pool) => {
            sqlx::query(INSERT)
                .bind(i64::from(slot))
                .bind(&key)
                .bind(proof.kind())
                .bind(answer.credential_id)
                .bind(answer.user.id)
                .bind(serialized)
                .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
                .bind(deadline.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
                .bind(proof.hash())
                .bind(i32::from(answer.home.get()))
                .execute(pool)
                .await?;
        }
    }
    cached(pool, &key).await
}
