use chrono::{DateTime, Utc};
use sqlx::FromRow;

/// A deploy key as stored, with the title and the read/write flag the columns hold as `name` and
/// `read_only`. It belongs to one repository, and `created_by_id` is the account its pushes are
/// attributed to.
#[derive(Debug, Clone, FromRow)]
pub struct DeployKey {
    pub id: i64,
    pub repo_id: i64,
    pub title: String,
    pub fingerprint: String,
    pub public_key: String,
    pub can_write: bool,
    pub created_by_id: Option<i64>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}
