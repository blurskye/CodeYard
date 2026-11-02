//! Stable imported author identities and explicit display mappings do not rewrite Git history.
use super::DbPool;
use anyhow::Result;

#[derive(sqlx::FromRow)]
pub struct Author {
    pub id: i64,
    pub repo_id: i64,
    pub author_name: String,
    pub author_email: String,
    pub mapped_name: Option<String>,
    pub mapped_email: Option<String>,
}

native_operation!(crate::backend::db::postgres::import_authors::record,
    crate::backend::db::sqlite::import_authors::record,
    fn record(pool: &DbPool, actor: i64, repo: i64, authors: &[(String, String)]) -> Result<Vec<Author>>);
native_operation!(crate::backend::db::postgres::import_authors::map,
    crate::backend::db::sqlite::import_authors::map,
    fn map(pool: &DbPool, actor: i64, repo: i64, author: i64, name: Option<&str>, email: Option<&str>) -> Result<Option<Author>>);
