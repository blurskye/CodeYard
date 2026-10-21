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
    /// The row's details as JSON text.
    pub details: String,
    pub created_at: DateTime<Utc>,
}
