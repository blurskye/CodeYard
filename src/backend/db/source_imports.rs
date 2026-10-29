//! Durable source-import attempts. A generation prevents an old worker from publishing a retry.
use super::DbPool;
use anyhow::Result;

#[derive(Clone, sqlx::FromRow)]
pub struct SourceImport {
    pub repo_id: i64,
    pub generation: String,
    pub vcs_url: String,
    pub status: String,
    pub updated_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportRefusal {
    #[error("Repository administration denied")]
    Denied,
    #[error("An import is already running")]
    Running,
}

pub const IMPORT_TIMEOUT_SECONDS: i64 = 1200;

impl SourceImport {
    pub fn state(&self) -> &str {
        if self.status == "importing" && !self.running() {
            "error"
        } else {
            &self.status
        }
    }

    pub fn running(&self) -> bool {
        self.status == "importing"
            && self.updated_at + IMPORT_TIMEOUT_SECONDS > chrono::Utc::now().timestamp()
    }
}

native_operation!(crate::backend::db::postgres::source_imports::get,
    crate::backend::db::sqlite::source_imports::get,
    fn get(pool: &DbPool, repo: i64) -> Result<Option<SourceImport>>);
native_operation!(crate::backend::db::postgres::source_imports::start,
    crate::backend::db::sqlite::source_imports::start,
    fn start(pool: &DbPool, actor: i64, repo: i64, url: &str, generation: &str) -> Result<SourceImport>);
native_operation!(crate::backend::db::postgres::source_imports::finish,
    crate::backend::db::sqlite::source_imports::finish,
    fn finish(pool: &DbPool, repo: i64, generation: &str, success: bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::source_imports::cancel,
    crate::backend::db::sqlite::source_imports::cancel,
    fn cancel(pool: &DbPool, actor: i64, repo: i64) -> Result<()>);
