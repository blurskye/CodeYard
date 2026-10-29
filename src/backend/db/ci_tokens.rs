//! Repository-scoped credentials issued only to an active CI claim.
use super::DbPool;
use anyhow::Result;

pub const BOT: &str = "github-actions[bot]";

pub struct NewJobToken<'a> {
    pub job: &'a str,
    pub hash: &'a str,
    pub permissions: &'a str,
    pub write: bool,
}

#[derive(sqlx::FromRow)]
pub struct JobToken {
    pub run_id: i64,
    pub repo_id: i64,
    pub permissions: String,
}

native_operation!(crate::backend::db::postgres::ci_tokens::issue,
    crate::backend::db::sqlite::ci_tokens::issue,
    fn issue(pool: &DbPool, run_id: i64, worker: &str, tokens: &[NewJobToken<'_>]) -> Result<()>);
native_operation!(crate::backend::db::postgres::ci_tokens::get,
    crate::backend::db::sqlite::ci_tokens::get,
    fn get(pool: &DbPool, token_id: i64) -> Result<Option<JobToken>>);
native_operation!(crate::backend::db::postgres::ci_tokens::finish_job,
    crate::backend::db::sqlite::ci_tokens::finish_job,
    fn finish_job(pool: &DbPool, run_id: i64, worker: &str, job: &str) -> Result<()>);
