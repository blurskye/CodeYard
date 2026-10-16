use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Milestone {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub title: String,
    pub description: Option<String>,
    /// The day the milestone is due, as YYYY-MM-DD.
    pub due_date: Option<String>,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// A milestone with its counts of open and closed issues and pull requests. The counts are read
/// from the items that name the milestone, not from any stored total.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MilestoneSummary {
    #[sqlx(flatten)]
    pub milestone: Milestone,
    pub open_items: i64,
    pub closed_items: i64,
}
