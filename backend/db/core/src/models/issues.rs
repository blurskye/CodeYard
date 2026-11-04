use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Issue {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub author_id: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub milestone_id: Option<i64>,
    pub closed_by_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct IssueComment {
    pub id: i64,
    pub issue_id: i64,
    pub author_id: i64,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
