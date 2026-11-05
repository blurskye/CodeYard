//! Directory publication runs while the caller retains its local namespace and authority locks.
use anyhow::{ensure, Result};
use codeyard_model::cells::{Key, KeyKind, Location, Object, ObjectKind};
use futures_util::future::BoxFuture;
use std::{fmt, sync::Arc, time::Duration};

#[derive(Clone, Copy)]
pub struct Config {
    pub request_seconds: u64,
    pub maximum_keys: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            request_seconds: 10,
            maximum_keys: 1000,
        }
    }
}
impl Config {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=60).contains(&self.request_seconds) && (1..=1000).contains(&self.maximum_keys),
            Refusal::Unavailable
        );
        Ok(())
    }
}

#[derive(Debug)]
pub enum Refusal {
    Conflict,
    Denied,
    Unavailable,
}
impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conflict => "Directory key is already claimed",
            Self::Denied => "Directory access denied",
            Self::Unavailable => "Directory is unavailable",
        })
    }
}
impl std::error::Error for Refusal {}

/// Only the authenticated client implements this boundary; cells never hold directory DB access.
pub trait Directory: Send + Sync {
    fn config(&self) -> Config;
    fn lookup<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Location>>>;
    fn reserve<'a>(
        &'a self,
        object: Object,
        keys: &'a [Key],
        nonce: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
    fn confirm<'a>(
        &'a self,
        object: Object,
        keys: &'a [Key],
        nonce: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
    fn release<'a>(&'a self, object: Object, keys: &'a [Key]) -> BoxFuture<'a, Result<()>>;
    fn forget(&self, object: Object) -> BoxFuture<'_, Result<()>>;
}
pub type Provider = Arc<dyn Directory>;

/// Names continue to belong to a surviving object after a rename, as redirects.
/// Emails and fingerprints instead follow their current native enrollment.
fn present_sql(object: ObjectKind, key: KeyKind) -> Result<&'static str> {
    Ok(match (object, key) {
        (ObjectKind::User, KeyKind::Name) => "SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND NOT is_stub AND CAST($2 AS TEXT) IS NOT NULL)",
        (ObjectKind::Organization, KeyKind::Name) => "SELECT EXISTS(SELECT 1 FROM organizations WHERE id=$1 AND NOT is_stub AND CAST($2 AS TEXT) IS NOT NULL)",
        (ObjectKind::User, KeyKind::Email) => "SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND NOT is_stub AND lower(email)=lower($2)) OR EXISTS(SELECT 1 FROM user_emails e JOIN users u ON u.id=e.user_id WHERE e.user_id=$1 AND NOT u.is_stub AND lower(e.email)=lower($2))",
        (ObjectKind::User, KeyKind::Ssh) => "SELECT EXISTS(SELECT 1 FROM ssh_keys k JOIN users u ON u.id=k.user_id WHERE k.user_id=$1 AND k.fingerprint=$2 AND NOT u.is_stub)",
        (ObjectKind::Repository, KeyKind::Ssh) => "SELECT EXISTS(SELECT 1 FROM deploy_keys k JOIN repositories r ON r.id=k.repo_id WHERE k.repo_id=$1 AND k.fingerprint=$2 AND NOT r.is_stub)",
        _ => anyhow::bail!(Refusal::Denied),
    })
}

fn account_keys(name: &str, email: Option<&str>, stub: bool) -> Result<Vec<Key>> {
    ensure!(!stub, Refusal::Denied);
    // This is a cell's internal CI principal, not a globally named user account.
    if name == crate::queries::ci_tokens::BOT {
        return Ok(Vec::new());
    }
    let mut keys = vec![Key::new(KeyKind::Name, name)?];
    if let Some(email) = email {
        keys.push(Key::new(KeyKind::Email, email)?);
    }
    Ok(keys)
}

pub fn user_keys(user: &crate::models::User) -> Result<Vec<Key>> {
    account_keys(&user.username, user.email.as_deref(), user.is_stub)
}
fn object_sql(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::User => "SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)",
        ObjectKind::Organization => "SELECT EXISTS(SELECT 1 FROM organizations WHERE id=$1)",
        ObjectKind::Repository => "SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1)",
    }
}

macro_rules! engine {
    ($module:ident, $pool:ty, $connection:ty, $begin:literal, $lock:path) => {
        pub mod $module {
            use super::*;
            async fn free_key(pool: &$pool, connection: &mut $connection, provider: &Provider, key: &Key, expected: Option<Object>) -> Result<bool> {
                let Some(previous) = provider.lookup(key).await? else { return Ok(true); };
                previous.object.validate().map_err(|_| Refusal::Unavailable)?;
                ensure!(key.belongs_to(previous.object.kind), Refusal::Unavailable);
                if Some(previous.object) == expected { return Ok(true); }
                if previous.cell == pool.cell_id() {
                    let present: bool = sqlx::query_scalar(present_sql(previous.object.kind, key.kind())?)
                        .bind(previous.object.id).bind(key.value()).fetch_one(&mut *connection).await?;
                    if !present {
                        if key.kind() == KeyKind::Name { provider.forget(previous.object).await?; }
                        else { provider.release(previous.object, std::slice::from_ref(key)).await?; }
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            /// Private signup review queues must also refuse keys already owned elsewhere.
            /// Activation still reserves them under the directory's unique constraint.
            pub async fn available(pool: &$pool, connection: &mut $connection, kind: ObjectKind, keys: &[Key]) -> Result<bool> {
                let Some(provider) = pool.directory() else { return Ok(true); };
                let config = provider.config();
                config.validate()?;
                ensure!(keys.len() <= config.maximum_keys && keys.iter().all(|key| key.belongs_to(kind)), Refusal::Denied);
                tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
                    for key in keys {
                        if !free_key(pool, connection, &provider, key, None).await? { return Ok(false); }
                    }
                    Ok(true)
                }).await.map_err(|_| Refusal::Unavailable)?
            }
            /// The caller already wrote its native row and still owns its local key locks.
            /// Confirmation precedes local commit, so an unconfirmed object cannot sign in.
            pub async fn publish(pool: &$pool, connection: &mut $connection, object: Object, keys: &[Key]) -> Result<bool> {
                let Some(provider) = pool.directory() else { return Ok(true); };
                let config = provider.config();
                config.validate()?;
                object.validate()?;
                ensure!(keys.len() <= config.maximum_keys && keys.iter().all(|key| key.belongs_to(object.kind)), Refusal::Denied);
                let result = tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
                    for key in keys {
                        ensure!(free_key(pool, connection, &provider, key, Some(object)).await?, Refusal::Conflict);
                    }
                    let nonce = codeyard_support::crypto::random_hex();
                    provider.reserve(object, keys, &nonce).await?;
                    provider.confirm(object, keys, &nonce).await
                }).await.map_err(|_| Refusal::Unavailable)?;
                match result {
                    Ok(()) => Ok(true),
                    Err(error) if matches!(error.downcast_ref::<Refusal>(), Some(Refusal::Conflict)) => Ok(false),
                    Err(error) => Err(error),
                }
            }
            /// Recheck absence under the same local key locks as enrollment. A concurrent
            /// re-enrollment by this object must not lose its new directory mapping.
            pub async fn release(pool: &$pool, object: Object, keys: &[Key]) -> Result<()> {
                let Some(provider) = pool.directory() else { return Ok(()); };
                let config = provider.config();
                config.validate()?;
                object.validate()?;
                ensure!(keys.len() <= config.maximum_keys && keys.iter().all(|key| key.kind()!=KeyKind::Name && key.belongs_to(object.kind)), Refusal::Denied);
                tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
                    let mut tx = pool.begin_with($begin).await?;
                    $lock(&mut tx, keys).await?;
                    let mut absent = Vec::new();
                    for key in keys {
                        let present: bool = sqlx::query_scalar(present_sql(object.kind, key.kind())?)
                            .bind(object.id).bind(key.value()).fetch_one(&mut *tx).await?;
                        if !present { absent.push(key.clone()); }
                    }
                    provider.release(object, &absent).await?;
                    tx.commit().await?;
                    Ok(())
                }).await.map_err(|_| Refusal::Unavailable)?
            }
            pub async fn forget(pool: &$pool, object: Object) -> Result<()> {
                let Some(provider) = pool.directory() else { return Ok(()); };
                let config = provider.config();
                config.validate()?;
                object.validate()?;
                tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
                    let present: bool = sqlx::query_scalar(object_sql(object.kind)).bind(object.id).fetch_one(pool).await?;
                    if !present { provider.forget(object).await?; }
                    Ok(())
                }).await.map_err(|_| Refusal::Unavailable)?
            }
            /// The native transaction already queued this effect. Failure leaves it for the worker.
            pub async fn finish_release(pool: &$pool, object: Object, keys: &[Key]) {
                if release(pool, object, keys).await.is_err() { tracing::warn!(object_id=object.id, "Directory key cleanup remains queued"); }
            }
            pub async fn finish_forget(pool: &$pool, object: Object) {
                if forget(pool, object).await.is_err() { tracing::warn!(object_id=object.id, "Directory object cleanup remains queued"); }
            }
            pub async fn user_id(pool: &$pool, connection: &mut $connection, user: i64, extra: &[Key]) -> Result<bool> {
                let Some(provider) = pool.directory() else { return Ok(true); };
                let config = provider.config();
                config.validate()?;
                let row: Option<(String, Option<String>, bool, i32)> = sqlx::query_as("SELECT substr(username,1,256),substr(email,1,256),is_stub,home_cell FROM users WHERE id=$1")
                    .bind(user).fetch_optional(&mut *connection).await?;
                let (name, email, stub, home) = row.ok_or(Refusal::Denied)?;
                ensure!(home == i32::from(pool.cell_id().get()), Refusal::Denied);
                let mut keys = account_keys(&name, email.as_deref(), stub)?;
                if keys.is_empty() { ensure!(extra.is_empty(), Refusal::Denied); return Ok(true); }
                ensure!(keys.len() <= config.maximum_keys && extra.len() <= config.maximum_keys.saturating_sub(keys.len()), Refusal::Unavailable);
                keys.extend_from_slice(extra);
                publish(pool, connection, Object { kind: ObjectKind::User, id: user }, &keys).await
            }
            pub async fn repository(pool: &$pool, connection: &mut $connection, repository: &crate::models::Repository) -> Result<()> {
                if pool.directory().is_none() { return Ok(()); }
                ensure!(!repository.is_stub && repository.home_cell == i32::from(pool.cell_id().get()), Refusal::Denied);
                ensure!(publish(pool, connection, Object { kind: ObjectKind::Repository, id: repository.id }, &[]).await?, Refusal::Conflict);
                Ok(())
            }
            pub async fn user(pool: &$pool, connection: &mut $connection, user: &crate::models::User) -> Result<bool> {
                if pool.directory().is_none() { return Ok(true); }
                ensure!(user.home_cell == i32::from(pool.cell_id().get()), Refusal::Denied);
                let keys = user_keys(user)?;
                if keys.is_empty() { return Ok(true); }
                publish(pool, connection, Object { kind: ObjectKind::User, id: user.id }, &keys).await
            }
        }
    };
}
engine!(
    postgres,
    crate::pool::PgPool,
    sqlx::PgConnection,
    "BEGIN",
    crate::queries::postgres::users::transitions::lock_directory_keys
);
engine!(
    sqlite,
    crate::pool::SqlitePool,
    sqlx::SqliteConnection,
    "BEGIN IMMEDIATE",
    crate::queries::sqlite::users::transitions::lock_directory_keys
);

/// Publish one bounded page while the operator owns the stopped-cell maintenance lock.
/// Rerunning a page is safe, including when a preceding TLS confirmation reply was lost.
pub async fn publish_native(
    pool: &crate::pool::PgPool,
    kind: ObjectKind,
    after: i64,
    limit: i64,
) -> Result<(i64, usize)> {
    ensure!(after >= 0 && (1..=250).contains(&limit), Refusal::Denied);
    let provider = pool.directory().ok_or(Refusal::Unavailable)?;
    let config = provider.config();
    config.validate()?;
    let table = match kind {
        ObjectKind::User => "users",
        ObjectKind::Organization => "organizations",
        ObjectKind::Repository => "repositories",
    };
    let ids: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id>$1 AND NOT is_stub AND home_cell=$2 ORDER BY id LIMIT $3"
    ))
    .bind(after)
    .bind(i32::from(pool.cell_id().get()))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for &id in &ids {
        let mut tx = pool.begin().await?;
        if kind == ObjectKind::User {
            crate::queries::postgres::users::transitions::account_locks(&mut tx, &[id], false)
                .await?;
        }
        let present: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT id FROM {table} WHERE id=$1 AND NOT is_stub AND home_cell=$2 FOR UPDATE"
        ))
        .bind(id)
        .bind(i32::from(pool.cell_id().get()))
        .fetch_optional(&mut *tx)
        .await?;
        if present.is_none() {
            continue;
        }
        let object = Object { kind, id };
        let keys = match kind {
            ObjectKind::User => {
                let rows: Vec<(String, String)> = sqlx::query_as("SELECT 'email',substr(email,1,256) FROM user_emails WHERE user_id=$1 UNION ALL SELECT 'ssh',substr(fingerprint,1,256) FROM ssh_keys WHERE user_id=$1 LIMIT $2")
                    .bind(id).bind((config.maximum_keys + 1) as i64).fetch_all(&mut *tx).await?;
                ensure!(rows.len() <= config.maximum_keys, Refusal::Unavailable);
                let keys = rows
                    .into_iter()
                    .map(|(kind, value)| {
                        Key::new(
                            if kind == "email" {
                                KeyKind::Email
                            } else {
                                KeyKind::Ssh
                            },
                            &value,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    postgres::user_id(pool, &mut tx, id, &keys).await?,
                    Refusal::Conflict
                );
                tx.commit().await?;
                continue;
            }
            ObjectKind::Organization => {
                let name: String =
                    sqlx::query_scalar("SELECT substr(name,1,256) FROM organizations WHERE id=$1")
                        .bind(id)
                        .fetch_one(&mut *tx)
                        .await?;
                vec![Key::new(KeyKind::Name, &name)?]
            }
            ObjectKind::Repository => {
                let rows: Vec<String> = sqlx::query_scalar("SELECT substr(fingerprint,1,256) FROM deploy_keys WHERE repo_id=$1 ORDER BY id LIMIT $2")
                    .bind(id).bind((config.maximum_keys + 1) as i64).fetch_all(&mut *tx).await?;
                ensure!(rows.len() <= config.maximum_keys, Refusal::Unavailable);
                rows.into_iter()
                    .map(|value| Key::new(KeyKind::Ssh, &value))
                    .collect::<Result<Vec<_>>>()?
            }
        };
        ensure!(
            postgres::publish(pool, &mut tx, object, &keys).await?,
            Refusal::Conflict
        );
        tx.commit().await?;
    }
    Ok((ids.last().copied().unwrap_or(after), ids.len()))
}
