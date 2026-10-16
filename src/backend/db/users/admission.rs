//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::users::admission::admit_password_attempt,
    crate::backend::db::sqlite::users::admission::admit_password_attempt,
    fn admit_password_attempt(pool: &DbPool, account_key: &str) -> Result<bool>);
