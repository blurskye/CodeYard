use chrono::{DateTime, Utc};
use sqlx::FromRow;

/// One audit log row, with the acting account's name when it still exists.
#[derive(Debug, Clone, FromRow)]
pub struct AuditEntry {
    pub id: i64,
    pub actor: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<i64>,
    /// The target's name: the account, the node, or the member an owner role went to.
    pub target_name: Option<String>,
    /// The organization the row belongs to, if any.
    pub org_name: Option<String>,
    /// The row's details as JSON text.
    pub details: String,
    pub created_at: DateTime<Utc>,
}
