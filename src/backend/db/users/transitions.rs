//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::User;
use crate::backend::db::DbPool;
use anyhow::Result;

/// Browser mutations retain the original session. Trusted local database callers use an ID.
pub struct AdministratorActor {
    pub(crate) user_id: i64,
    pub(crate) session: Option<String>,
}

impl From<i64> for AdministratorActor {
    fn from(user_id: i64) -> Self {
        Self {
            user_id,
            session: None,
        }
    }
}

impl From<(i64, String)> for AdministratorActor {
    fn from((user_id, session): (i64, String)) -> Self {
        Self {
            user_id,
            session: Some(session),
        }
    }
}

#[derive(Debug)]
pub enum SetupClaim {
    Completed(Box<User>),
    AlreadyCompleted,
    ExistingAccounts,
}

#[derive(Debug)]
pub enum BanChange {
    Updated(Box<User>),
    Denied,
    SelfBan,
    LastAdmin,
    Missing,
}

#[derive(Debug)]
pub enum EmailChange {
    Updated(Box<User>),
    /// Another account already uses this address.
    Taken,
    Missing,
}

#[derive(Debug)]
pub enum RoleChange {
    Updated(Box<User>),
    Denied,
    SelfChange,
    Missing,
}

#[derive(Debug)]
pub enum AccountAdmission {
    User(Box<User>),
    Requested,
    SetupRequired,
    ApprovalRequired,
    Denied,
    Conflict,
}

native_operation!(crate::backend::db::postgres::users::transitions::complete_setup,
    crate::backend::db::sqlite::users::transitions::complete_setup,
    fn complete_setup(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    hash: &str,
    mode: &str,
    recovery: Option<(i64, &str)>,
) -> Result<SetupClaim>);

native_operation!(crate::backend::db::postgres::users::transitions::create_cli_user,
    crate::backend::db::sqlite::users::transitions::create_cli_user,
    fn create_cli_user(pool: &DbPool, username: &str, email: Option<&str>) -> Result<AccountAdmission>);

native_operation!(crate::backend::db::postgres::users::transitions::bootstrap_admin,
    crate::backend::db::sqlite::users::transitions::bootstrap_admin,
    /// Local operator recovery only: creates a new admin without adopting existing accounts.
    fn bootstrap_admin(pool: &DbPool, username: &str, hash: &str, mode: &str) -> Result<SetupClaim>);

native_operation!(crate::backend::db::postgres::users::transitions::change_ban,
    crate::backend::db::sqlite::users::transitions::change_ban,
    fn change_ban(pool: &DbPool, actor: impl Into<AdministratorActor>, target: i64, banned: bool) -> Result<BanChange>);

native_operation!(crate::backend::db::postgres::users::transitions::change_email,
    crate::backend::db::sqlite::users::transitions::change_email,
    fn change_email(pool: &DbPool, user_id: i64, email: &str, expected: &str, session: &str) -> Result<EmailChange>);

native_operation!(crate::backend::db::postgres::users::transitions::change_admin_role,
    crate::backend::db::sqlite::users::transitions::change_admin_role,
    fn change_admin_role(pool: &DbPool, actor: impl Into<AdministratorActor>, target: i64, admin: bool) -> Result<RoleChange>);

native_operation!(crate::backend::db::postgres::users::transitions::register_password,
    crate::backend::db::sqlite::users::transitions::register_password,
    fn register_password(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    hash: &str,
    administrator: Option<(i64, &str, bool)>,
) -> Result<AccountAdmission>);

native_operation!(crate::backend::db::postgres::users::transitions::admit_oauth_account,
    crate::backend::db::sqlite::users::transitions::admit_oauth_account,
    fn admit_oauth_account(
    pool: &DbPool,
    provider: &str,
    provider_id: &str,
    username_base: &str,
    display_name: Option<&str>,
) -> Result<AccountAdmission>);

native_operation!(crate::backend::db::postgres::users::transitions::enroll_oauth_identity,
    crate::backend::db::sqlite::users::transitions::enroll_oauth_identity,
    fn enroll_oauth_identity(
    pool: &DbPool,
    actor: i64,
    user: i64,
    provider: &str,
    subject: &str,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::users::transitions::compare_exchange_password,
    crate::backend::db::sqlite::users::transitions::compare_exchange_password,
    fn compare_exchange_password(
    pool: &DbPool,
    user_id: i64,
    expected: &str,
    replacement: &str,
    clear_must_change: bool,
    revoke_sessions: bool,
    session: Option<&str>,
) -> Result<Option<User>>);
