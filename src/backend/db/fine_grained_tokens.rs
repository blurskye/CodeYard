//! Fine-grained personal access tokens, their repository selections and organization policies.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

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

pub const MAX_REPOSITORIES: usize = 100;

pub struct RepositoryChoices {
    pub selected: Vec<(i64, String)>,
    pub matching: Vec<(i64, String)>,
}

native_operation!(crate::backend::db::postgres::fine_grained_tokens::repository_choices, crate::backend::db::sqlite::fine_grained_tokens::repository_choices,
    fn repository_choices(pool: &DbPool, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, org: Option<i64>, query: &str, selected: &[i64]) -> Result<RepositoryChoices>);

pub struct NewToken<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub owner_org_id: Option<i64>,
    pub repository_access: &'a str,
    pub permissions: &'a str,
    pub repositories: &'a [i64],
    pub scope: &'a str,
    pub expires_at: Option<DateTime<Utc>>,
}

native_operation!(crate::backend::db::postgres::fine_grained_tokens::create, crate::backend::db::sqlite::fine_grained_tokens::create,
    /// Inserts the token while `session` is a live session of `user`. An organization owner
    /// needs the user's membership and a policy that allows fine-grained tokens; the token
    /// waits for review when the policy asks for approval and the user is not an owner.
    /// Returns the token id and its status.
    fn create(pool: &DbPool, user: i64, session: &str, hash: &str, token: &NewToken<'_>) -> Result<Option<(i64, String)>>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::regenerate, crate::backend::db::sqlite::fine_grained_tokens::regenerate,
    /// Replaces the hash and expiry while `session` is a live session of `user`.
    fn regenerate(pool: &DbPool, user: i64, session: &str, token: i64, hash: &str, expires_at: Option<DateTime<Utc>>) -> Result<bool>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::get, crate::backend::db::sqlite::fine_grained_tokens::get,
    fn get(pool: &DbPool, token: i64) -> Result<Option<Token>>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::list, crate::backend::db::sqlite::fine_grained_tokens::list,
    /// The user's newest 100 fine-grained tokens.
    fn list(pool: &DbPool, user: i64, before: i64) -> Result<Vec<Token>>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::bounds, crate::backend::db::sqlite::fine_grained_tokens::bounds,
    fn bounds(pool: &DbPool, token: i64) -> Result<Option<Bounds>>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::repositories, crate::backend::db::sqlite::fine_grained_tokens::repositories,
    /// The selected repositories' ids and names, at most 100.
    fn repositories(pool: &DbPool, token: i64) -> Result<Vec<(i64, String)>>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::selected, crate::backend::db::sqlite::fine_grained_tokens::selected,
    fn selected(pool: &DbPool, token: i64, repo: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::policy, crate::backend::db::sqlite::fine_grained_tokens::policy,
    /// The organization's policy, or the defaults when it has none.
    fn policy(pool: &DbPool, org: i64) -> Result<Policy>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::set_policy, crate::backend::db::sqlite::fine_grained_tokens::set_policy,
    /// Saves the policy while `actor` owns the organization; false when they do not.
    fn set_policy(pool: &DbPool, actor: i64, org: i64, policy: &Policy) -> Result<bool>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::organization_tokens, crate::backend::db::sqlite::fine_grained_tokens::organization_tokens,
    /// The organization's newest 1000 tokens with `status` whose users are still members.
    fn organization_tokens(pool: &DbPool, org: i64, status: &str) -> Result<Vec<Token>>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::review, crate::backend::db::sqlite::fine_grained_tokens::review,
    /// Moves the organization's tokens in `from` to `status` while `reviewer` owns the
    /// organization, and returns how many changed.
    fn review(pool: &DbPool, org: i64, reviewer: i64, tokens: &[i64], from: &str, status: &str, reason: Option<&str>) -> Result<u64>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::classic_refused, crate::backend::db::sqlite::fine_grained_tokens::classic_refused,
    /// Whether `token` is a classic personal token and `org` refuses classic tokens. OAuth,
    /// App, job and fine-grained tokens are never classic.
    fn classic_refused(pool: &DbPool, token: i64, org: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::fine_grained_tokens::classic_refused_repository, crate::backend::db::sqlite::fine_grained_tokens::classic_refused_repository,
    /// `classic_refused` for the organization that owns `repo`, if any.
    fn classic_refused_repository(pool: &DbPool, token: i64, repo: i64) -> Result<bool>);
