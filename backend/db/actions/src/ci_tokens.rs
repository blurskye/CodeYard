use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

pub struct NewJobToken<'a> {
    pub job: &'a str,
    pub hash: &'a str,
    pub permissions: &'a str,
    pub write: bool,
}

native_operation!(crate::postgres::ci_tokens::issue,
    crate::sqlite::ci_tokens::issue,
    fn issue(pool: &DbPool, run_id: i64, worker: &str, tokens: &[NewJobToken<'_>]) -> Result<()>);
native_operation!(codeyard_db_core::queries::postgres::ci_tokens::get,
    codeyard_db_core::queries::sqlite::ci_tokens::get,
    fn get(pool: &DbPool, token_id: i64) -> Result<Option<JobToken>>);
native_operation!(crate::postgres::ci_tokens::finish_job,
    crate::sqlite::ci_tokens::finish_job,
    fn finish_job(pool: &DbPool, run_id: i64, worker: &str, job: &str) -> Result<()>);

use codeyard_db_core::queries::ci_tokens::JobToken;

use codeyard_db_core::queries::ci_tokens::JobToken;
