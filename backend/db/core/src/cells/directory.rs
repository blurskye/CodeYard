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
        Self { request_seconds: 10, maximum_keys: 1000 }
    }
}
impl Config {
    pub fn validate(self) -> Result<()> {
        ensure!((1..=60).contains(&self.request_seconds) && (1..=1000).contains(&self.maximum_keys), Refusal::Unavailable);
        Ok(())
    }
}

#[derive(Debug)]
pub enum Refusal { Conflict, Denied, Unavailable }
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
    fn reserve<'a>(&'a self, object: Object, keys: &'a [Key], nonce: &'a str) -> BoxFuture<'a, Result<()>>;
    fn confirm<'a>(&'a self, object: Object, keys: &'a [Key], nonce: &'a str) -> BoxFuture<'a, Result<()>>;
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

pub fn user_keys(user: &crate::models::User) -> Result<Vec<Key>> {
    ensure!(!user.is_stub, Refusal::Denied);
    // This is a cell's internal CI principal, not a globally named user account.
    if user.username == crate::queries::ci_tokens::BOT { return Ok(Vec::new()); }
    let mut keys = vec![Key::new(KeyKind::Name, &user.username)?];
    if let Some(email) = &user.email { keys.push(Key::new(KeyKind::Email, email)?); }
    Ok(keys)
}

macro_rules! engine {
    ($module:ident, $pool:ty, $connection:ty) => {
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
            pub async fn user(pool: &$pool, connection: &mut $connection, user: &crate::models::User) -> Result<bool> {
                if pool.directory().is_none() { return Ok(true); }
                let keys = user_keys(user)?;
                if keys.is_empty() { return Ok(true); }
                publish(pool, connection, Object { kind: ObjectKind::User, id: user.id }, &keys).await
            }
        }
    };
}
engine!(postgres, crate::pool::PgPool, sqlx::PgConnection);
engine!(sqlite, crate::pool::SqlitePool, sqlx::SqliteConnection);
