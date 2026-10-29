//! CodeQL archive metadata; object bytes use the shared object store.
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(sqlx::FromRow)]
pub struct Database {
    pub id: i64,
    pub repo_id: i64,
    pub language: String,
    pub name: String,
    pub uploader_id: Option<i64>,
    pub object_key: String,
    pub size: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct NewDatabase<'a> {
    pub repo: i64,
    pub actor: i64,
    pub language: &'a str,
    pub name: &'a str,
    pub key: &'a str,
    pub size: i64,
}

native_operation!(super::postgres::codeql::list, super::sqlite::codeql::list,
    fn list(pool: &DbPool, repo: i64) -> Result<Vec<Database>>);
native_operation!(super::postgres::codeql::get, super::sqlite::codeql::get,
    fn get(pool: &DbPool, repo: i64, language: &str) -> Result<Option<Database>>);
native_operation!(super::postgres::codeql::stage, super::sqlite::codeql::stage,
    fn stage(pool: &DbPool, actor: i64, repo: i64, key: &str) -> Result<()>);
native_operation!(super::postgres::codeql::publish, super::sqlite::codeql::publish,
    fn publish(pool: &DbPool, input: &NewDatabase<'_>) -> Result<Database>);
native_operation!(super::postgres::codeql::delete, super::sqlite::codeql::delete,
    fn delete(pool: &DbPool, actor: i64, repo: i64, language: &str) -> Result<()>);
