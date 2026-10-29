//! REST request budgets are shared by credentials for an account and by database-backed nodes.
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug, sqlx::FromRow)]
pub struct Budget {
    pub used: i64,
    pub reset_at: i64,
}

pub struct Request<'a> {
    pub key: &'a str,
    pub resource: &'a str,
    pub now: i64,
    pub limit: i64,
    pub seconds: i64,
    pub consume: bool,
}

native_operation!(crate::backend::db::postgres::api_limits::budget,
    crate::backend::db::sqlite::api_limits::budget,
    fn budget(pool: &DbPool, request: Request<'_>) -> Result<(Budget, bool)>);
