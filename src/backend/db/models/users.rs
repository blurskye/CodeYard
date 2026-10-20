use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    pub avatar_file_id: Option<Uuid>,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
    pub is_admin: bool,
    pub is_banned: bool,
    pub must_change_password: bool,
    pub email_verified: bool,
    pub two_factor_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl User {
    /// The no-reply address that names this account without publishing its email.
    pub fn noreply_email(&self) -> String {
        format!("{}@users.noreply", self.username)
    }

    /// The email that commits made for this account carry: its own, or the no-reply address
    /// when it has none.
    pub fn commit_email(&self) -> String {
        self.email
            .as_ref()
            .filter(|email| !email.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| self.noreply_email())
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct AdminUser {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub is_banned: bool,
    pub must_change_password: bool,
    pub two_factor_enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct AdminAccountSummary {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub is_banned: bool,
    pub must_change_password: bool,
    pub two_factor_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub total_files: i64,
    pub total_bytes: i64,
    /// Commits in recorded pushes attributed to this account.
    pub commits_total: i64,
    pub pushes_total: i64,
    pub contributions_total: i64,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct AdminActivityPoint {
    pub day: NaiveDate,
    pub daily_contributions: i64,
    pub cumulative_contributions: i64,
    pub daily_storage_bytes: i64,
    pub cumulative_storage_bytes: i64,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct NodeConfiguration {
    pub id: i16,
    pub node_mode: String,
    pub setup_completed: bool,
    /// `allow` or `block`: whether AI training crawlers are blocked.
    pub ai_crawlers: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SignupRequest {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub status: String,
    pub reviewed_by: Option<i64>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct BrowserSession {
    pub id: String,
    pub user_id: Option<i64>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct OAuthState {
    pub browser_binding: Option<String>,
    pub state: String,
    pub pkce_verifier: String,
    pub provider: String,
    pub redirect_uri: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PersonalAccessToken {
    pub id: i64,
    #[serde(skip_serializing)]
    pub user_id: i64,
    pub name: String,
    pub scope: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SshKey {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub fingerprint: String,
    pub public_key: String,
    pub key_type: String,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UserWithKey {
    pub user: User,
    pub key: SshKey,
}
