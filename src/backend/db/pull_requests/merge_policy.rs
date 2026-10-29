//! A checked merge retains its policy epoch and cancellation record through Git publication.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoMergeIdentity {
    pub pull_request_id: i64,
    pub enabled_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: i64,
    pub auto_merge: Option<AutoMergeIdentity>,
}

pub enum Lease {
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
}
impl Lease {
    pub async fn release(self) -> Result<()> {
        match self {
            Self::Postgres(tx) => tx.rollback().await?,
            Self::Sqlite(tx) => tx.rollback().await?,
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Merge policy or auto-merge consent changed before publication")]
pub struct Changed;

native_operation!(crate::backend::db::postgres::pull_requests::merge_policy::revision,
    crate::backend::db::sqlite::pull_requests::merge_policy::revision,
    fn revision(pool: &DbPool, repo: i64) -> Result<i64>);
native_operation!(crate::backend::db::postgres::pull_requests::merge_policy::retain,
    crate::backend::db::sqlite::pull_requests::merge_policy::retain,
    fn retain(pool: &DbPool, repo: i64, expected: &Snapshot) -> Result<Lease>);
