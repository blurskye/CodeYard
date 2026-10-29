use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::FromRow;

#[derive(Clone, Debug, FromRow)]
pub struct AsyncMerge {
    pub uuid: String,
    pub repo_id: i64,
    pub pull_request_id: i64,
    pub token_id: Option<i64>,
    pub worker_node_id: Option<i64>,
    pub owner: String,
    pub name: String,
    pub number: i32,
    pub request: Value,
    pub status: String,
    pub details: Value,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}
pub struct NewAsyncMerge<'a> {
    pub uuid: &'a str,
    pub repo_id: i64,
    pub pull_request_id: i64,
    pub token_id: i64,
    pub worker_node_id: Option<i64>,
    pub owner: &'a str,
    pub name: &'a str,
    pub number: i32,
    pub request: &'a Value,
    pub details: &'a Value,
}
native_operation!(crate::backend::db::postgres::pull_requests::async_merge::insert,
    crate::backend::db::sqlite::pull_requests::async_merge::insert,
    fn insert(pool:&DbPool,input:NewAsyncMerge<'_>)->Result<bool>);
native_operation!(crate::backend::db::postgres::pull_requests::async_merge::get,
    crate::backend::db::sqlite::pull_requests::async_merge::get,
    fn get(pool:&DbPool,repo_id:i64,pull_request_id:i64,uuid:&str)->Result<Option<AsyncMerge>>);
native_operation!(crate::backend::db::postgres::pull_requests::async_merge::finish,
    crate::backend::db::sqlite::pull_requests::async_merge::finish,
    fn finish(pool:&DbPool,uuid:&str,status:&str,details:&Value)->Result<()>);
native_operation!(crate::backend::db::postgres::pull_requests::async_merge::recover,
    crate::backend::db::sqlite::pull_requests::async_merge::recover,
    fn recover(pool:&DbPool,node_id:Option<i64>,details:&Value)->Result<u64>);
native_operation!(crate::backend::db::postgres::pull_requests::async_merge::prune,
    crate::backend::db::sqlite::pull_requests::async_merge::prune,
    fn prune(pool:&DbPool)->Result<u64>);
