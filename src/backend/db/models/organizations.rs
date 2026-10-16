use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Organization {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub avatar_file_id: Option<Uuid>,
    #[serde(skip_serializing)]
    pub billing_email: Option<String>,
    pub is_verified: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrgRole {
    Owner,
    Admin,
    Member,
}

// Persisted roles are authority, not a presentation fallback. Unknown values deny.
impl std::str::FromStr for OrgRole {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            _ => Err(()),
        }
    }
}

impl OrgRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrgRole::Owner => "owner",
            OrgRole::Admin => "admin",
            OrgRole::Member => "member",
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct OrgMembership {
    pub id: i64,
    pub org_id: i64,
    pub user_id: i64,
    pub role: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Team {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub privacy: String,
    // Unsupported legacy hierarchy is persistence only, never safe presentation.
    #[serde(skip_serializing)]
    pub parent_team_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TeamMembership {
    pub id: i64,
    pub team_id: i64,
    pub user_id: i64,
    pub role: String,
    pub created_at: DateTime<Utc>,
}
