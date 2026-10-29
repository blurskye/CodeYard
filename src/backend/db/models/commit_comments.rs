use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CommitComment {
    pub id: i64,
    pub repo_id: i64,
    pub author_id: Option<i64>,
    pub sha: String,
    pub body: String,
    pub path: Option<String>,
    pub position: Option<i32>,
    pub line: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
