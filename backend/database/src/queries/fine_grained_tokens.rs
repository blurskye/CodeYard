use chrono::DateTime;
use chrono::Utc;
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Token {
    pub token_id: i64,
    pub user_id: i64,
    pub name: String,
    pub description: String,
    pub owner_org_id: Option<i64>,
    pub repository_access: String,
    pub permissions: String,
    pub status: String,
    pub reason: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub requested_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub last_used_ip: Option<String>,
}

/// Portable policy predicate for trusted SQL parameter/column expressions, never request values.
/// Non-classic credentials retain their own grant checks; an absent policy allows classic tokens.
pub fn classic_allowed_condition(token: &str, organization: &str) -> String {
    let token = format!("CAST({token} AS BIGINT)");
    format!("NOT EXISTS(SELECT 1 FROM organization_token_policies policy WHERE policy.org_id={organization} AND NOT policy.classic_allowed AND NOT EXISTS(SELECT 1 FROM oauth_app_tokens WHERE token_id={token}) AND NOT EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id={token}) AND NOT EXISTS(SELECT 1 FROM fine_grained_tokens WHERE token_id={token}) AND NOT EXISTS(SELECT 1 FROM actions_github_tokens WHERE token_id={token}))")
}

/// A token's bounds with the live organization facts its access depends on.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Bounds {
    pub token_id: i64,
    pub user_id: i64,
    pub owner_org_id: Option<i64>,
    pub repository_access: String,
    pub permissions: String,
    pub status: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub requested_at: DateTime<Utc>,
    /// The user is still a member of the owning organization (always true for a user owner).
    pub member: bool,
    pub fine_grained_allowed: bool,
    pub max_lifetime_days: Option<i32>,
}

impl Bounds {
    /// False while the organization has not approved the token, after the user left the
    /// organization, or when its policy no longer allows the token or its lifetime.
    pub fn active(&self) -> bool {
        self.status == "approved"
            && (self.owner_org_id.is_none()
                || (self.member
                    && self.fine_grained_allowed
                    && within(
                        self.expires_at,
                        self.requested_at,
                        self.max_lifetime_days.map(i64::from),
                    )))
    }
}

/// An expiry fits a maximum of `days` when it is no later than the end of the day `days`
/// after `from`, which is how the token form picks dates. `None` days allow no expiry.
pub fn within(expires_at: Option<DateTime<Utc>>, from: DateTime<Utc>, days: Option<i64>) -> bool {
    if !crate::queries::users::tokens::supported_expiry(expires_at) {
        return false;
    }
    match (days, expires_at) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(days), Some(expiry)) => expiry <= from + chrono::Duration::days(days + 1),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Policy {
    pub fine_grained_allowed: bool,
    pub fine_grained_approval: bool,
    pub classic_allowed: bool,
    pub max_lifetime_days: Option<i32>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            fine_grained_allowed: true,
            fine_grained_approval: true,
            classic_allowed: true,
            max_lifetime_days: Some(366),
        }
    }
}
