use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

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

pub const UPLOAD_QUOTA: &str = "codeql_database";

pub struct NewDatabase<'a> {
    /// The retained quota charge, confirmed in the publication transaction.
    pub charge: uuid::Uuid,
    pub repo: i64,
    pub actor: codeyard_database::queries::credentials::Actor,
    pub language: &'a str,
    pub name: &'a str,
    pub key: &'a str,
    pub size: i64,
}

native_operation!(crate::postgres::codeql::list, crate::sqlite::codeql::list,
    fn list(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64) -> Result<Vec<Database>>);
native_operation!(crate::postgres::codeql::get, crate::sqlite::codeql::get,
    fn get(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64, language: &str) -> Result<Option<Database>>);
native_operation!(crate::postgres::codeql::stage, crate::sqlite::codeql::stage,
    fn stage(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64, key: &str) -> Result<()>);
native_operation!(crate::postgres::codeql::publish, crate::sqlite::codeql::publish,
    fn publish(pool: &DbPool, input: &NewDatabase<'_>) -> Result<Database>);
native_operation!(crate::postgres::codeql::delete, crate::sqlite::codeql::delete,
    fn delete(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, repo: i64, language: &str) -> Result<()>);
