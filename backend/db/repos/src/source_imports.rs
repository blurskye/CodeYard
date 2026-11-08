use anyhow::Result;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

#[derive(Clone, sqlx::FromRow)]
pub struct SourceImport {
    pub repo_id: i64,
    pub generation: String,
    pub vcs_url: String,
    pub status: String,
    pub updated_at: i64,
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

native_operation!(crate::postgres::source_imports::get,
    crate::sqlite::source_imports::get,
    fn get(pool: &DbPool, repo: i64) -> Result<Option<SourceImport>>);
native_operation!(crate::postgres::source_imports::start,
    crate::sqlite::source_imports::start,
    fn start(pool: &DbPool, actor: i64, repo: i64, url: &str, generation: &str) -> Result<SourceImport>);
native_operation!(crate::postgres::source_imports::finish,
    crate::sqlite::source_imports::finish,
    fn finish(pool: &DbPool, repo: i64, generation: &str, success: bool) -> Result<()>);
native_operation!(crate::postgres::source_imports::cancel,
    crate::sqlite::source_imports::cancel,
    fn cancel(pool: &DbPool, actor: i64, repo: i64) -> Result<()>);
