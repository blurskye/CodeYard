use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct File {
    pub id: Uuid,
    pub user_id: Option<i64>,
    pub filename: String,
    pub original_name: String,
    pub content_type: String,
    pub file_size: i64,
    pub sha256_hex: String,
    pub object_key: String,
    pub reason: Option<String>,
    pub visibility: String,
    /// Set while the upload is pending: when the reservation lapses.
    pub upload_expires_at: Option<DateTime<Utc>>,
    /// Retained after confirmation so deletion outlives every issued PUT URL.
    pub put_expires_at: Option<DateTime<Utc>>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl File {
    /// An upload that has not been confirmed and whose reservation has not lapsed.
    pub fn is_pending(&self) -> bool {
        !self.is_active
            && self
                .upload_expires_at
                .is_some_and(|expires| expires > Utc::now())
    }
}

/// Generic uploads have no repository/issue ACL. Only explicitly public avatars
/// may be declassified here; other assets remain owner-only until scoped APIs exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileVisibility {
    Owner,
    Public,
}

impl FileVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Public => "public",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReasonLimit {
    pub reason: String,
    pub max_file_size_bytes: i64,
    pub max_uploads_per_hour: i32,
    pub max_bytes_per_hour: i64,
    pub max_uploads_per_day: i32,
    pub max_bytes_per_day: i64,
    pub allowed_content_types: Option<Vec<String>>,
    pub requires_auth: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UserFileStats {
    pub user_id: i64,
    pub username: String,
    pub total_files: i64,
    pub total_bytes: i64,
    pub avatar_count: i64,
    pub release_asset_count: i64,
    pub issue_attachment_count: i64,
    pub pr_attachment_count: i64,
    pub readme_image_count: i64,
    pub repo_asset_count: i64,
    pub org_avatar_count: i64,
    pub other_count: i64,
    pub uploads_24h: i64,
    pub bytes_24h: i64,
    pub downloads_24h: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileReason {
    Avatar,
    OrgAvatar,
    ReleaseAsset,
    IssueAttachment,
    PullRequestAttachment,
    ReadmeImage,
    RepoAsset,
    Other,
}

impl FileReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::OrgAvatar => "org_avatar",
            Self::ReleaseAsset => "release_asset",
            Self::IssueAttachment => "issue_attachment",
            Self::PullRequestAttachment => "pr_attachment",
            Self::ReadmeImage => "readme_image",
            Self::RepoAsset => "repo_asset",
            Self::Other => "other",
        }
    }
}

impl std::str::FromStr for FileReason {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "avatar" => Ok(Self::Avatar),
            "org_avatar" => Ok(Self::OrgAvatar),
            "release_asset" => Ok(Self::ReleaseAsset),
            "issue_attachment" => Ok(Self::IssueAttachment),
            "pr_attachment" => Ok(Self::PullRequestAttachment),
            "readme_image" => Ok(Self::ReadmeImage),
            "repo_asset" => Ok(Self::RepoAsset),
            "other" => Ok(Self::Other),
            _ => Err(()),
        }
    }
}
