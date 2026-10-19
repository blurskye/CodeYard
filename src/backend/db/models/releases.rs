use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// A release as stored. The tag names a commit; the release itself is metadata about it.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Release {
    pub id: i64,
    pub repo_id: i64,
    pub tag_name: String,
    pub target_commitish: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub is_draft: bool,
    pub is_prerelease: bool,
    pub is_latest: bool,
    pub author_id: i64,
    pub created_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
}

/// A release with its author's login, as the list and the page show it.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ReleaseRow {
    #[sqlx(flatten)]
    pub release: Release,
    pub author: String,
}

/// A file attached to a release. The bytes live in the uploaded-file store under `file_id`.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub id: i64,
    pub release_id: i64,
    pub file_id: Uuid,
    pub name: String,
    pub label: Option<String>,
    pub content_type: String,
    pub size: i64,
    pub download_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
