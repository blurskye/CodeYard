//! A mention of an account homed in another cell reaches that account's home inbox. The
//! repository's cell decides who may read, so only public repositories send notices, and the
//! home stores only public metadata stubs.
use super::repositories::Owner;
use crate::models::{Repository, User};
use crate::DbPool;
use anyhow::{ensure, Result};
use codeyard_model::cells::{Key, KeyKind, Object, ObjectKind};
use codeyard_support::cells::Prefix;
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

/// Checks the issuing cell against the directory's current homes of the repository and the
/// recipient.
pub trait Admission: Send + Sync {
    fn check<'a>(&'a self, event: &'a Event)
        -> futures_util::future::BoxFuture<'a, Result<Prefix>>;
}
