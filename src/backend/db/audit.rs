//! Shared named operation surface; native transactions stay inside db.
use anyhow::Result;
use codeyard_db_core::models::AuditEntry;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(crate::backend::db::postgres::audit::page,
    crate::backend::db::sqlite::audit::page,
    fn page(pool: &DbPool, authority: super::users::transitions::AdministratorActor, action: &str, actor: &str, page: i64, limit: i64) -> Result<Option<(i64, Vec<AuditEntry>)>>);

native_operation!(crate::backend::db::postgres::audit::count_audit_entries,
    crate::backend::db::sqlite::audit::count_audit_entries,
    /// Rows whose action contains `action` and whose actor name contains `actor`, ignoring case.
    /// Empty text matches every row.
    fn count_audit_entries(pool: &DbPool, action: &str, actor: &str) -> Result<i64>);

native_operation!(crate::backend::db::postgres::audit::list_audit_entries,
    crate::backend::db::sqlite::audit::list_audit_entries,
    /// The same rows as `count_audit_entries`, newest first.
    fn list_audit_entries(
    pool: &DbPool,
    action: &str,
    actor: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditEntry>>);
