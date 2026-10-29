//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{AdminAccountSummary, AdminActivityPoint, AdminUser, User};
use crate::backend::db::DbPool;
use anyhow::Result;

/// A settings mutation retains the live credential while it changes account data.
pub enum AccountCredential<'a> {
    Session(&'a str),
    Token(i64),
}

/// Identity plus the credential that admitted an operation. Numeric IDs are reserved for
/// trusted internal callers; HTTP adapters must retain the original session or token.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AccountActor {
    Trusted(i64),
    Session { user_id: i64, session: String },
    Token { user_id: i64, token_id: i64 },
}

impl std::fmt::Debug for AccountActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AccountActor([redacted])")
    }
}

impl From<i64> for AccountActor {
    fn from(id: i64) -> Self { Self::Trusted(id) }
}

impl AccountActor {
    pub fn user_id(&self) -> i64 {
        match self {
            Self::Trusted(id) => *id,
            Self::Session { user_id, .. } | Self::Token { user_id, .. } => *user_id,
        }
    }

    pub fn credential(&self) -> Option<AccountCredential<'_>> {
        match self {
            Self::Trusted(_) => None,
            Self::Session { session, .. } => Some(AccountCredential::Session(session)),
            Self::Token { token_id, .. } => Some(AccountCredential::Token(*token_id)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("A live unrestricted account credential is required")]
pub struct AccountRestricted;

native_operation!(crate::backend::db::postgres::users::accounts::create_user,
    crate::backend::db::sqlite::users::accounts::create_user,
    fn create_user(pool: &DbPool, username: &str, email: Option<&str>) -> Result<User>);

native_operation!(crate::backend::db::postgres::users::accounts::create_user_with_password,
    crate::backend::db::sqlite::users::accounts::create_user_with_password,
    fn create_user_with_password(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
) -> Result<User>);

native_operation!(crate::backend::db::postgres::users::accounts::create_user_with_password_and_flags,
    crate::backend::db::sqlite::users::accounts::create_user_with_password_and_flags,
    fn create_user_with_password_and_flags(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
    is_admin: bool,
    must_change_password: bool,
) -> Result<User>);

native_operation!(crate::backend::db::postgres::users::accounts::count_users,
    crate::backend::db::sqlite::users::accounts::count_users,
    fn count_users(pool: &DbPool) -> Result<i64>);

native_operation!(crate::backend::db::postgres::users::accounts::replace_password_hash,
    crate::backend::db::sqlite::users::accounts::replace_password_hash,
    /// Swaps in a hash of the same password made at a newer cost, if the stored hash is still
    /// `old`. Sessions stay signed in. Returns whether the hash was replaced.
    fn replace_password_hash(pool: &DbPool, user_id: i64, old: &str, new: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::users::accounts::update_user_password,
    crate::backend::db::sqlite::users::accounts::update_user_password,
    fn update_user_password(
    pool: &DbPool,
    user_id: i64,
    password_hash: &str,
    must_change_password: bool,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::users::accounts::get_user_by_id,
    crate::backend::db::sqlite::users::accounts::get_user_by_id,
    fn get_user_by_id(pool: &DbPool, id: i64) -> Result<Option<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::get_user_by_username,
    crate::backend::db::sqlite::users::accounts::get_user_by_username,
    fn get_user_by_username(pool: &DbPool, username: &str) -> Result<Option<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::get_user_by_email,
    crate::backend::db::sqlite::users::accounts::get_user_by_email,
    fn get_user_by_email(pool: &DbPool, email: &str) -> Result<Option<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::get_users_by_emails,
    crate::backend::db::sqlite::users::accounts::get_users_by_emails,
    fn get_users_by_emails(pool: &DbPool, emails: &[String]) -> Result<Vec<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::get_verified_email_owners,
    crate::backend::db::sqlite::users::accounts::get_verified_email_owners,
    fn get_verified_email_owners(pool: &DbPool, emails: &[String]) -> Result<Vec<(String, i64)>>);

native_operation!(crate::backend::db::postgres::users::accounts::get_users_by_usernames,
    crate::backend::db::sqlite::users::accounts::get_users_by_usernames,
    fn get_users_by_usernames(pool: &DbPool, usernames: &[String]) -> Result<Vec<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::get_users_by_ids,
    crate::backend::db::sqlite::users::accounts::get_users_by_ids,
    fn get_users_by_ids(pool: &DbPool, ids: &[i64]) -> Result<Vec<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::list_users_since,
    crate::backend::db::sqlite::users::accounts::list_users_since,
    fn list_users_since(pool: &DbPool, since: i64, limit: i64) -> Result<Vec<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::list_users,
    crate::backend::db::sqlite::users::accounts::list_users,
    fn list_users(pool: &DbPool) -> Result<Vec<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::create_user_from_oauth,
    crate::backend::db::sqlite::users::accounts::create_user_from_oauth,
    /// Creates a user from an OAuth profile. Avatars are uploaded files
    /// (`users.avatar_file_id`), so none is set. The email is marked verified
    /// when the provider gave one.
    fn create_user_from_oauth(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    display_name: Option<&str>,
) -> Result<User>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAvatarOutcome {
    Applied(Option<uuid::Uuid>),
    NotUsable,
    /// The account was banned after the request authenticated.
    Banned,
}

native_operation!(crate::backend::db::postgres::users::accounts::set_user_avatar,
    crate::backend::db::sqlite::users::accounts::set_user_avatar,
    fn set_user_avatar(
    pool: &DbPool,
    user_id: i64,
    avatar_file_id: Option<uuid::Uuid>,
) -> Result<UserAvatarOutcome>);

native_operation!(crate::backend::db::postgres::users::accounts::update_user_profile,
    crate::backend::db::sqlite::users::accounts::update_user_profile,
    fn update_user_profile(
    pool: &DbPool,
    user_id: i64,
    session: &str,
    display_name: Option<&str>,
    bio: Option<&str>,
    location: Option<&str>,
    website: Option<&str>,
) -> Result<Option<User>>);

native_operation!(crate::backend::db::postgres::users::accounts::count_matching_users,
    crate::backend::db::sqlite::users::accounts::count_matching_users,
    fn count_matching_users(pool: &DbPool, text: &str) -> Result<(i64, i64, i64)>);
native_operation!(crate::backend::db::postgres::users::accounts::list_admin_users,
    crate::backend::db::sqlite::users::accounts::list_admin_users,
    fn list_admin_users(pool: &DbPool, text: &str, role: &str, limit: i64, offset: i64) -> Result<Vec<AdminUser>>);

native_operation!(crate::backend::db::postgres::users::accounts::list_admin_account_summaries,
    crate::backend::db::sqlite::users::accounts::list_admin_account_summaries,
    fn list_admin_account_summaries(pool: &DbPool, limit: i64, offset: i64) -> Result<Vec<AdminAccountSummary>>);

native_operation!(crate::backend::db::postgres::users::accounts::list_admin_activity_points,
    crate::backend::db::sqlite::users::accounts::list_admin_activity_points,
    fn list_admin_activity_points(
    pool: &DbPool,
    days: i32,
) -> Result<Vec<AdminActivityPoint>>);

native_operation!(crate::backend::db::postgres::users::accounts::count_unbanned_admin_users,
    crate::backend::db::sqlite::users::accounts::count_unbanned_admin_users,
    fn count_unbanned_admin_users(pool: &DbPool) -> Result<i64>);

/// Public author identity only; no email, credentials, restrictions or storage.
#[derive(Debug, Clone)]
pub struct AuthorDisplay {
    pub id: i64,
    pub username: String,
    pub avatar_file_id: Option<uuid::Uuid>,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
    pub is_admin: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
