use anyhow::Result;
use codeyard_database::models::User;
use codeyard_database::native_operation;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::DbPool;

native_operation!(crate::postgres::organizations::blocks::list, crate::sqlite::organizations::blocks::list,
    fn list(pool: &DbPool, actor: i64, org: i64, after: i64, window: (i64,i64)) -> Result<OrganizationOutcome<Vec<User>>>);
native_operation!(crate::postgres::organizations::blocks::check, crate::sqlite::organizations::blocks::check,
    fn check(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<bool>>);
native_operation!(crate::postgres::organizations::blocks::change, crate::sqlite::organizations::blocks::change,
    fn change(pool: &DbPool, actor: impl Into<codeyard_database::queries::credentials::Actor>, org: i64, target: i64, blocked: bool) -> Result<OrganizationOutcome<()>>);
