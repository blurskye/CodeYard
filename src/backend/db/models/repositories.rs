use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnerKind {
    User,
    Org,
}

impl From<&str> for OwnerKind {
    fn from(s: &str) -> Self {
        match s {
            "org" => OwnerKind::Org,
            _ => OwnerKind::User,
        }
    }
}

impl OwnerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OwnerKind::User => "user",
            OwnerKind::Org => "org",
        }
    }
}

/// A repository that another user shares with `user_id`, with that user's access level.
#[derive(Debug, Clone, FromRow)]
pub struct SharedRepository {
    #[sqlx(flatten)]
    pub repository: Repository,
    pub access_level: i16,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Repository {
    pub id: i64,
    pub owner_kind: String,
    pub owner_user_id: Option<i64>,
    pub owner_org_id: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub website: Option<String>,
    pub is_private: bool,
    pub is_archived: bool,
    pub wiki_enabled: bool,
    pub is_fork: bool,
    pub forked_from_id: Option<i64>,
    pub default_branch: String,
    pub allow_merge_commit: bool,
    pub allow_squash_merge: bool,
    pub allow_rebase_merge: bool,
    pub disk_path: String,
    pub storage_node_id: Option<i64>,
    pub size_bytes: i64,
    pub stars_count: i32,
    pub forks_count: i32,
    pub watchers_count: i32,
    pub open_issues_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Repository {
    pub fn owner_kind(&self) -> OwnerKind {
        OwnerKind::from(self.owner_kind.as_str())
    }

    /// Numeric presentation identifier only; user and organization IDs may overlap.
    /// Never use this value to authorize a user or select a notification recipient.
    pub fn owner_id(&self) -> i64 {
        self.owner_user_id.or(self.owner_org_id).unwrap_or(0)
    }

    /// A personal owner is a user only when the persisted owner kind says so.
    /// Unknown kinds fail closed rather than inheriting OwnerKind's display fallback.
    pub fn personal_owner_id(&self) -> Option<i64> {
        if self.owner_kind == "user" {
            self.owner_user_id
        } else {
            None
        }
    }

    pub fn is_personal_owner(&self, user_id: i64) -> bool {
        self.personal_owner_id() == Some(user_id)
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RepoAccess {
    pub id: i64,
    pub user_id: i64,
    pub repo_id: i64,
    pub access_level: i16,
    pub created_at: DateTime<Utc>,
}

impl RepoAccess {
    pub fn level(&self) -> AccessLevel {
        AccessLevel::from(self.access_level)
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TeamRepoAccess {
    pub id: i64,
    pub team_id: i64,
    pub repo_id: i64,
    pub permission: i16,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchProtectionRule {
    pub id: i64,
    pub repo_id: i64,
    pub pattern: String,
    pub require_pull_request: bool,
    pub required_approving_reviews: i32,
    pub dismiss_stale_reviews: bool,
    pub require_status_checks: bool,
    pub required_status_checks: Vec<String>,
    pub require_up_to_date_branch: bool,
    pub require_linear_history: bool,
    pub block_force_pushes: bool,
    pub block_deletions: bool,
    pub restrict_pushes: bool,
    pub push_allowances: Vec<i64>,
    /// Users who may force push although `block_force_pushes` refuses everyone else.
    pub force_push_allowances: Vec<i64>,
    /// When false, administrators skip this rule's direct-write and merge checks.
    pub enforce_admins: bool,
    pub require_conversation_resolution: bool,
    /// Each changed file with code owners needs an approval from one of its owners.
    pub require_code_owner_review: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Label {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RepoLanguage {
    pub id: i64,
    pub repo_id: i64,
    pub language: String,
    pub bytes: i64,
    pub percentage: f64,
    pub color: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RepoContributor {
    pub id: i64,
    pub repo_id: i64,
    pub user_id: Option<i64>,
    pub email: String,
    pub name: String,
    pub commits_count: i32,
    pub additions: i32,
    pub deletions: i32,
    pub avatar_url: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct RepoSecret {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
    pub encrypted_value: Vec<u8>,
    pub nonce: Vec<u8>,
    pub created_by_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSecretMeta {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(i16)]
pub enum AccessLevel {
    None = 0,
    Read = 1,
    Triage = 2,
    Write = 3,
    Maintain = 4,
    Admin = 5,
    Owner = 6,
}

impl AccessLevel {
    pub fn can_read(&self) -> bool {
        *self >= AccessLevel::Read
    }
    pub fn can_triage(&self) -> bool {
        *self >= AccessLevel::Triage
    }
    pub fn can_write(&self) -> bool {
        *self >= AccessLevel::Write
    }
    pub fn can_maintain(&self) -> bool {
        *self >= AccessLevel::Maintain
    }
    pub fn can_admin(&self) -> bool {
        *self >= AccessLevel::Admin
    }
    pub fn is_owner(&self) -> bool {
        *self == AccessLevel::Owner
    }
}

impl From<i16> for AccessLevel {
    fn from(value: i16) -> Self {
        match value {
            0 => AccessLevel::None,
            1 => AccessLevel::Read,
            2 => AccessLevel::Triage,
            3 => AccessLevel::Write,
            4 => AccessLevel::Maintain,
            5 => AccessLevel::Admin,
            6 => AccessLevel::Owner,
            _ => AccessLevel::None,
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Star {
    pub user_id: i64,
    pub repo_id: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Watch {
    pub user_id: i64,
    pub repo_id: i64,
    pub level: String,
    pub events: String,
    pub created_at: DateTime<Utc>,
}
