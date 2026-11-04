use anyhow::Result;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::DbPool;

native_operation!(crate::postgres::organizations::outside::list, crate::sqlite::organizations::outside::list,
    fn list(pool: &DbPool, actor: i64, org: i64, filter: &str, after: i64, window: (i64,i64)) -> Result<OrganizationOutcome<Vec<User>>>);
native_operation!(crate::postgres::organizations::outside::remove, crate::sqlite::organizations::outside::remove,
    fn remove(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, org: i64, target: i64) -> Result<OrganizationOutcome<()>>);
