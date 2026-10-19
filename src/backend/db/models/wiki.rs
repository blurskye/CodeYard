use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// One saved version of a wiki page. The body is empty in listings and set when one revision is read.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WikiRevision {
    pub page_id: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub message: String,
    pub author_id: i64,
    pub author: String,
    pub saved_at: DateTime<Utc>,
}

/// A page with its head revision: the content readers see.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WikiPage {
    pub id: i64,
    pub repo_id: i64,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    #[sqlx(flatten)]
    pub head: WikiRevision,
}

/// A page as the page list shows it.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WikiPageSummary {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub updated_at: DateTime<Utc>,
}
