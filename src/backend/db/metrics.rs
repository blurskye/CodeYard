//! Bounded monitoring reads, without repository or account labels.
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use sqlx::FromRow;

pub const MAX_STORAGE_NODES: usize = 1000;

#[derive(Debug, FromRow)]
pub struct StorageNode {
    pub node_id: i64,
    pub repository_bytes: i64,
    pub disk_free: Option<i64>,
}

#[derive(Debug, FromRow)]
pub struct CiQueue {
    pub queued: i64,
    pub oldest_seconds: i64,
}

#[derive(Debug, FromRow)]
pub struct NodeCount {
    pub status: String,
    pub count: i64,
}

native_operation!(crate::backend::db::postgres::metrics::storage_nodes, crate::backend::db::sqlite::metrics::storage_nodes,
    fn storage_nodes(pool: &DbPool) -> Result<Vec<StorageNode>>);
native_operation!(crate::backend::db::postgres::metrics::replica_lag, crate::backend::db::sqlite::metrics::replica_lag,
    fn replica_lag(pool: &DbPool) -> Result<i64>);
native_operation!(crate::backend::db::postgres::metrics::ci_queue, crate::backend::db::sqlite::metrics::ci_queue,
    fn ci_queue(pool: &DbPool) -> Result<CiQueue>);
native_operation!(crate::backend::db::postgres::metrics::node_counts, crate::backend::db::sqlite::metrics::node_counts,
    fn node_counts(pool: &DbPool) -> Result<Vec<NodeCount>>);
