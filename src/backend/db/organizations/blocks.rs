use super::OrganizationOutcome;
use crate::backend::db::{models::User, DbPool};
use anyhow::Result;

native_operation!(crate::backend::db::postgres::organizations::blocks::list, crate::backend::db::sqlite::organizations::blocks::list,
    fn list(pool: &DbPool, actor: i64, org: i64, after: i64, window: (i64,i64)) -> Result<OrganizationOutcome<Vec<User>>>);
native_operation!(crate::backend::db::postgres::organizations::blocks::check, crate::backend::db::sqlite::organizations::blocks::check,
    fn check(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<bool>>);
native_operation!(crate::backend::db::postgres::organizations::blocks::change, crate::backend::db::sqlite::organizations::blocks::change,
    fn change(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, target: i64, blocked: bool) -> Result<OrganizationOutcome<()>>);
