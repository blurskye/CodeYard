use anyhow::Result;
use codeyard_database::models::AuditEntry;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

native_operation!(crate::postgres::audit::page,
    crate::sqlite::audit::page,
    fn page(pool: &DbPool, authority: crate::users::transitions::AdministratorActor, action: &str, actor: &str, page: i64, limit: i64) -> Result<Option<(i64, Vec<AuditEntry>)>>);

native_operation!(crate::postgres::audit::count_audit_entries,
    crate::sqlite::audit::count_audit_entries,
    /// Rows whose action contains `action` and whose actor name contains `actor`, ignoring case.
    /// Empty text matches every row.
    fn count_audit_entries(pool: &DbPool, action: &str, actor: &str) -> Result<i64>);

native_operation!(crate::postgres::audit::list_audit_entries,
    crate::sqlite::audit::list_audit_entries,
    /// The same rows as `count_audit_entries`, newest first.
    fn list_audit_entries(
    pool: &DbPool,
    action: &str,
    actor: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditEntry>>);
