use anyhow::Result;
use codeyard_db_core::models::AdminAccountSummary;
use codeyard_db_core::models::AdminActivityPoint;
use codeyard_db_core::models::AdminUser;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_core::DbPool;

pub struct AdminUserPage {
    pub counts: (i64, i64, i64),
    pub page: usize,
    pub users: Vec<AdminUser>,
}

native_operation!(crate::postgres::users::accounts::admin_users_page,
    crate::sqlite::users::accounts::admin_users_page,
    fn admin_users_page(pool: &DbPool, administrator: &crate::users::transitions::AdministratorActor, text: &str, role: &str, limit: i64, page: usize) -> Result<AdminUserPage>);

native_operation!(crate::postgres::users::accounts::create_user,
    crate::sqlite::users::accounts::create_user,
    fn create_user(pool: &DbPool, username: &str, email: Option<&str>) -> Result<User>);

native_operation!(crate::postgres::users::accounts::create_user_with_password,
    crate::sqlite::users::accounts::create_user_with_password,
    fn create_user_with_password(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
) -> Result<User>);

native_operation!(crate::postgres::users::accounts::create_user_with_password_and_flags,
    crate::sqlite::users::accounts::create_user_with_password_and_flags,
    fn create_user_with_password_and_flags(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
    is_admin: bool,
    must_change_password: bool,
) -> Result<User>);

native_operation!(crate::postgres::users::accounts::count_users,
    crate::sqlite::users::accounts::count_users,
    fn count_users(pool: &DbPool) -> Result<i64>);

native_operation!(crate::postgres::users::accounts::replace_password_hash,
    crate::sqlite::users::accounts::replace_password_hash,
    /// Swaps in a hash of the same password made at a newer cost, if the stored hash is still
    /// `old`. Sessions stay signed in. Returns whether the hash was replaced.
    fn replace_password_hash(pool: &DbPool, user_id: i64, old: &str, new: &str) -> Result<bool>);

native_operation!(crate::postgres::users::accounts::update_user_password,
    crate::sqlite::users::accounts::update_user_password,
    fn update_user_password(
    pool: &DbPool,
    user_id: i64,
    password_hash: &str,
    must_change_password: bool,
) -> Result<()>);

native_operation!(crate::postgres::users::accounts::get_user_by_id,
    crate::sqlite::users::accounts::get_user_by_id,
    fn get_user_by_id(pool: &DbPool, id: i64) -> Result<Option<User>>);

native_operation!(crate::postgres::users::accounts::get_user_by_username,
    crate::sqlite::users::accounts::get_user_by_username,
    fn get_user_by_username(pool: &DbPool, username: &str) -> Result<Option<User>>);

native_operation!(crate::postgres::users::accounts::get_user_by_email,
    crate::sqlite::users::accounts::get_user_by_email,
    fn get_user_by_email(pool: &DbPool, email: &str) -> Result<Option<User>>);

/// An input Git address and its selected account from the same statement snapshot.
#[derive(sqlx::FromRow)]
pub struct CommitAuthor {
    pub address: String,
    #[sqlx(flatten)]
    pub user: User,
}

native_operation!(crate::postgres::users::accounts::commit_authors,
    crate::sqlite::users::accounts::commit_authors,
    fn commit_authors(pool: &DbPool, emails: &[String]) -> Result<Vec<CommitAuthor>>);

native_operation!(crate::postgres::users::accounts::get_users_by_usernames,
    crate::sqlite::users::accounts::get_users_by_usernames,
    fn get_users_by_usernames(pool: &DbPool, usernames: &[String]) -> Result<Vec<User>>);

native_operation!(crate::postgres::users::accounts::get_users_by_ids,
    crate::sqlite::users::accounts::get_users_by_ids,
    fn get_users_by_ids(pool: &DbPool, ids: &[i64]) -> Result<Vec<User>>);

native_operation!(crate::postgres::users::accounts::list_users_since,
    crate::sqlite::users::accounts::list_users_since,
    fn list_users_since(pool: &DbPool, since: i64, limit: i64) -> Result<Vec<User>>);

native_operation!(crate::postgres::users::accounts::list_users,
    crate::sqlite::users::accounts::list_users,
    fn list_users(pool: &DbPool) -> Result<Vec<User>>);

native_operation!(crate::postgres::users::accounts::create_user_from_oauth,
    crate::sqlite::users::accounts::create_user_from_oauth,
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
}

native_operation!(crate::postgres::users::accounts::set_user_avatar,
    crate::sqlite::users::accounts::set_user_avatar,
    /// Fails with `AccountRestricted` when the account or `credential` is no longer live.
    fn set_user_avatar(
    pool: &DbPool,
    user_id: i64,
    credential: &AccountCredential<'_>,
    avatar_file_id: Option<uuid::Uuid>,
) -> Result<UserAvatarOutcome>);

native_operation!(crate::postgres::users::accounts::update_user_profile,
    crate::sqlite::users::accounts::update_user_profile,
    fn update_user_profile(
    pool: &DbPool,
    user_id: i64,
    session: &str,
    display_name: Option<&str>,
    bio: Option<&str>,
    location: Option<&str>,
    website: Option<&str>,
) -> Result<Option<User>>);

native_operation!(crate::postgres::users::accounts::count_matching_users,
    crate::sqlite::users::accounts::count_matching_users,
    fn count_matching_users(pool: &DbPool, text: &str) -> Result<(i64, i64, i64)>);
native_operation!(crate::postgres::users::accounts::list_admin_users,
    crate::sqlite::users::accounts::list_admin_users,
    fn list_admin_users(pool: &DbPool, text: &str, role: &str, limit: i64, offset: i64) -> Result<Vec<AdminUser>>);

native_operation!(crate::postgres::users::accounts::list_admin_account_summaries,
    crate::sqlite::users::accounts::list_admin_account_summaries,
    fn list_admin_account_summaries(pool: &DbPool, limit: i64, offset: i64) -> Result<Vec<AdminAccountSummary>>);

native_operation!(crate::postgres::users::accounts::list_admin_activity_points,
    crate::sqlite::users::accounts::list_admin_activity_points,
    fn list_admin_activity_points(
    pool: &DbPool,
    days: i32,
) -> Result<Vec<AdminActivityPoint>>);

native_operation!(crate::postgres::users::accounts::count_unbanned_admin_users,
    crate::sqlite::users::accounts::count_unbanned_admin_users,
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
