//! Outside collaborators are nonmembers with explicit access to organization repositories.
use super::OrganizationOutcome;
use crate::backend::db::{models::User, DbPool};
use anyhow::Result;

native_operation!(crate::backend::db::postgres::organizations::outside::list, crate::backend::db::sqlite::organizations::outside::list,
    fn list(pool: &DbPool, actor: i64, org: i64, filter: &str, after: i64, window: (i64,i64)) -> Result<OrganizationOutcome<Vec<User>>>);
native_operation!(crate::backend::db::postgres::organizations::outside::remove, crate::backend::db::sqlite::organizations::outside::remove,
    fn remove(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);
