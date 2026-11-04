use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

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

native_operation!(crate::postgres::api_limits::budget,
    crate::sqlite::api_limits::budget,
    fn budget(pool: &DbPool, request: Request<'_>) -> Result<(Budget, bool)>);
