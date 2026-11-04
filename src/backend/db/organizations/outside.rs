//! Outside collaborators are nonmembers with explicit access to organization repositories.
use super::OrganizationOutcome;
use anyhow::Result;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(crate::backend::db::postgres::organizations::outside::list, crate::backend::db::sqlite::organizations::outside::list,
    fn list(pool: &DbPool, actor: i64, org: i64, filter: &str, after: i64, window: (i64,i64)) -> Result<OrganizationOutcome<Vec<User>>>);
native_operation!(crate::backend::db::postgres::organizations::outside::remove, crate::backend::db::sqlite::organizations::outside::remove,
    fn remove(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);
