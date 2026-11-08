//! A mention of an account homed in another cell reaches that account's home inbox. The
//! repository's cell decides who may read, so only public repositories send notices, and the
//! home stores only public metadata stubs.
use super::repositories::Owner;
use crate::models::{Repository, User};
use crate::DbPool;
use anyhow::{ensure, Result};
use codeyard_model::cells::{Key, KeyKind, Object, ObjectKind};
use codeyard_types::cells::Prefix;
use serde::{Deserialize, Serialize};

pub const TOPIC: &str = "notice";
/// Longest mention text a notice carries; the sender cuts longer text at a character boundary.
pub const MAXIMUM_BODY: usize = 65536;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub user_id: i64,
    pub repository: Repository,
    pub owner: Owner,
    pub actor: User,
    pub number: i32,
    pub subject: String,
    pub title: String,
    pub body: String,
    pub link: String,
}
fn text(value: &str, maximum: usize) -> bool {
    !value.contains('\0') && value.len() <= maximum
}
impl Notice {
    /// `home` is the repository's cell, as the directory reports it.
    pub fn validate(&self, home: Prefix) -> Result<()> {
        Prefix::for_id(self.user_id)?;
        Prefix::for_id(self.actor.id)?;
        super::repositories::validate_public(&self.repository, &self.owner, home)?;
        ensure!(
            !self.repository.is_private && !self.repository.is_internal,
            "Only public repositories send notices"
        );
        super::authorization::validate_public_user(&self.actor)?;
        Key::new(KeyKind::Name, &self.actor.username)?;
        ensure!(
            self.user_id != self.actor.id
                && self.number > 0
                && !self.subject.is_empty()
                && text(&self.subject, 4096)
                && !self.title.is_empty()
                && text(&self.title, 4096)
                && text(&self.body, MAXIMUM_BODY)
                && text(&self.link, 2048)
                && self.link.starts_with('/')
                && !self.link.starts_with("//"),
            "Invalid notice"
        );
        Ok(())
    }
    pub fn repository_object(&self) -> Object {
        Object {
            kind: ObjectKind::Repository,
            id: self.repository.id,
        }
    }
    pub fn recipient(&self) -> Object {
        Object {
            kind: ObjectKind::User,
            id: self.user_id,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: i64,
    pub notice: Notice,
    /// Withdrawal only narrows visibility; it never recreates a delivered notice.
    #[serde(default)]
    pub withdrawn: bool,
}
impl Event {
    pub fn validate(&self, home: Prefix) -> Result<()> {
        Prefix::for_id(self.id)?;
        self.notice.validate(home)
    }
}

const QUEUE: &str =
    "INSERT INTO cell_outbox(root_kind,root_id,topic,payload) VALUES('repository',$1,$2,$3)";

/// Queues the notice for at-least-once delivery. A cell without a directory sends nothing.
pub async fn queue(pool: &DbPool, notice: &Notice) -> Result<bool> {
    if pool.directory().is_none() {
        return Ok(false);
    }
    notice.validate(pool.cell_id())?;
    let payload = serde_json::to_string(notice)?;
    ensure!(payload.len() <= 256 * 1024, "Notice exceeds its limit");
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query(QUEUE)
                .bind(notice.repository.id)
                .bind(TOPIC)
                .bind(payload)
                .execute(pool)
                .await?;
        }
        DbPool::Sqlite(pool) => {
            sqlx::query(QUEUE)
                .bind(notice.repository.id)
                .bind(TOPIC)
                .bind(payload)
                .execute(pool)
                .await?;
        }
    }
    Ok(true)
}

/// Stores the repository, its owner and a same-home actor as stubs, and returns the actor's
/// local row. An actor homed in a third cell is named only when this cell already has its row.
pub async fn retain(pool: &DbPool, home: Prefix, notice: &Notice) -> Result<Option<i64>> {
    notice.validate(home)?;
    super::repositories::retain_public(pool, home, &notice.repository, &notice.owner).await?;
    let actor = &notice.actor;
    if !actor.is_stub && actor.home_cell == i32::from(home.get()) {
        super::stubs::user(pool, home, actor).await?;
    }
    const KNOWN: &str = "SELECT id FROM users WHERE id=$1 AND home_cell=$2";
    let known: Option<i64> = match pool {
        DbPool::Postgres(pool) => {
            sqlx::query_scalar(KNOWN)
                .bind(actor.id)
                .bind(actor.home_cell)
                .fetch_optional(pool)
                .await?
        }
        DbPool::Sqlite(pool) => {
            sqlx::query_scalar(KNOWN)
                .bind(actor.id)
                .bind(actor.home_cell)
                .fetch_optional(pool)
                .await?
        }
    };
    Ok(known)
}

/// A stub is metadata, never proof that the source is still public. Missing providers,
/// timeouts and unavailable homes fail closed, including email and streaming delivery.
pub async fn readable(provider: Option<&super::authorization::Provider>, repository: i64) -> bool {
    readable_until(
        provider,
        repository,
        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
    )
    .await
}

pub async fn readable_until(
    provider: Option<&super::authorization::Provider>,
    repository: i64,
    deadline: tokio::time::Instant,
) -> bool {
    let Some(provider) = provider else {
        return false;
    };
    if tokio::time::Instant::now() >= deadline {
        return false;
    }
    tokio::time::timeout_at(deadline, provider.notice_public(repository))
        .await
        .is_ok_and(|result| result.unwrap_or(false))
}

pub async fn public(pool: &DbPool, repository: i64) -> Result<bool> {
    const SQL: &str = "SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND NOT is_stub AND NOT is_private AND NOT is_internal AND NOT is_gist)";
    Ok(match pool {
        DbPool::Postgres(pool) => {
            sqlx::query_scalar(SQL)
                .bind(repository)
                .fetch_one(pool)
                .await?
        }
        DbPool::Sqlite(pool) => {
            sqlx::query_scalar(SQL)
                .bind(repository)
                .fetch_one(pool)
                .await?
        }
    })
}

/// A withdrawal is monotone and idempotent. Retain the scope row so deletion cannot turn
/// a repository notice into an unscoped account notice.
pub async fn withdraw(pool: &DbPool, home: Prefix, repository: i64) -> Result<()> {
    const SQL: &str =
        "UPDATE repositories SET is_private=TRUE WHERE id=$1 AND is_stub AND home_cell=$2";
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query(SQL)
                .bind(repository)
                .bind(i32::from(home.get()))
                .execute(pool)
                .await?;
        }
        DbPool::Sqlite(pool) => {
            sqlx::query(SQL)
                .bind(repository)
                .bind(i32::from(home.get()))
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Checks the issuing cell against the directory's current homes of the repository and the
/// recipient.
pub trait Admission: Send + Sync {
    fn check<'a>(&'a self, event: &'a Event)
        -> futures_util::future::BoxFuture<'a, Result<Prefix>>;
}
