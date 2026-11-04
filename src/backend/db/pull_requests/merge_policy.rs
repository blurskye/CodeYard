//! A checked merge confirms, right before Git publication, that its policy epoch and auto-merge
//! consent are still the ones it was judged under.
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoMergeIdentity {
    pub pull_request_id: i64,
    pub generation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: i64,
    pub auto_merge: Option<AutoMergeIdentity>,
}

#[derive(Debug, thiserror::Error)]
#[error("Merge policy or auto-merge consent changed before publication")]
pub struct Changed;

native_operation!(crate::backend::db::postgres::pull_requests::merge_policy::revision,
    crate::backend::db::sqlite::pull_requests::merge_policy::revision,
    fn revision(pool: &DbPool, repo: i64) -> Result<i64>);
native_operation!(crate::backend::db::postgres::pull_requests::merge_policy::check,
    crate::backend::db::sqlite::pull_requests::merge_policy::check,
    /// Fails with [`Changed`] when the repository's policy epoch or the auto-merge consent moved
    /// since `expected` was read. It holds nothing afterwards: the caller publishes outside any
    /// transaction.
    fn check(pool: &DbPool, repo: i64, expected: &Snapshot) -> Result<()>);
