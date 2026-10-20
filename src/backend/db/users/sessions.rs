//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{BrowserSession, OAuthState, Session, User};
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

native_operation!(crate::backend::db::postgres::users::sessions::create_session,
    crate::backend::db::sqlite::users::sessions::create_session,
    fn create_session(
    pool: &DbPool,
    session_id: &str,
    user_id: i64,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<Session>);

native_operation!(crate::backend::db::postgres::users::sessions::get_session,
    crate::backend::db::sqlite::users::sessions::get_session,
    fn get_session(pool: &DbPool, session_id: &str) -> Result<Option<Session>>);

native_operation!(crate::backend::db::postgres::users::sessions::get_session_account,
    crate::backend::db::sqlite::users::sessions::get_session_account,
    fn get_session_account(pool: &DbPool, session_id: &str) -> Result<Option<User>>);

native_operation!(crate::backend::db::postgres::users::sessions::get_session_user,
    crate::backend::db::sqlite::users::sessions::get_session_user,
    fn get_session_user(pool: &DbPool, session_id: &str) -> Result<Option<User>>);

native_operation!(crate::backend::db::postgres::users::sessions::delete_session,
    crate::backend::db::sqlite::users::sessions::delete_session,
    fn delete_session(pool: &DbPool, session_id: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::users::sessions::save_oauth_state,
    crate::backend::db::sqlite::users::sessions::save_oauth_state,
    fn save_oauth_state(
    pool: &DbPool,
    state: &str,
    pkce_verifier: &str,
    provider: &str,
    redirect_uri: Option<&str>,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::users::sessions::get_oauth_state,
    crate::backend::db::sqlite::users::sessions::get_oauth_state,
    fn get_oauth_state(pool: &DbPool, state: &str) -> Result<Option<OAuthState>>);

native_operation!(crate::backend::db::postgres::users::sessions::save_bound_oauth_state,
    crate::backend::db::sqlite::users::sessions::save_bound_oauth_state,
    fn save_bound_oauth_state(
    pool: &DbPool,
    state: &str,
    verifier: &str,
    provider: &str,
    redirect: &str,
    binding: &str,
    return_to: &str,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::users::sessions::consume_bound_oauth_state,
    crate::backend::db::sqlite::users::sessions::consume_bound_oauth_state,
    fn consume_bound_oauth_state(
    pool: &DbPool,
    state: &str,
    provider: &str,
    binding: &str,
    redirect: &str,
) -> Result<Option<OAuthState>>);

native_operation!(crate::backend::db::postgres::users::sessions::get_browser_session,
    crate::backend::db::sqlite::users::sessions::get_browser_session,
    fn get_browser_session(pool: &DbPool, id: &str) -> Result<Option<BrowserSession>>);

/// The proof used to issue a session. Second factors are consumed with the session insert.
#[derive(Clone, Copy)]
pub enum SessionCredential<'a> {
    Password(&'a str),
    SecondFactor {
        password: &'a str,
        recovery_hash: Option<&'a str>,
        totp: Option<(&'a [u8], i64)>,
    },
    OAuth,
}

impl<'a> From<Option<&'a str>> for SessionCredential<'a> {
    fn from(password: Option<&'a str>) -> Self {
        match password {
            Some(password) => Self::Password(password),
            None => Self::OAuth,
        }
    }
}

impl<'a> SessionCredential<'a> {
    pub(crate) fn password(self) -> Option<&'a str> {
        match self {
            Self::Password(password) | Self::SecondFactor { password, .. } => Some(password),
            Self::OAuth => None,
        }
    }
}

pub async fn rotate_authenticated_session<'a>(
    pool: &DbPool,
    old: Option<&str>,
    fresh: &str,
    user_id: i64,
    credential: impl Into<SessionCredential<'a>>,
    user_agent: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<Option<BrowserSession>> {
    let credential = credential.into();
    match pool {
        DbPool::Postgres(pool) => {
            crate::backend::db::postgres::users::sessions::rotate_authenticated_session(
                pool, old, fresh, user_id, credential, user_agent, expires_at,
            )
            .await
        }
        DbPool::Sqlite(pool) => {
            crate::backend::db::sqlite::users::sessions::rotate_authenticated_session(
                pool, old, fresh, user_id, credential, user_agent, expires_at,
            )
            .await
        }
    }
}

// The account's sessions that have not expired.
native_operation!(crate::backend::db::postgres::users::sessions::list_user_sessions,
    crate::backend::db::sqlite::users::sessions::list_user_sessions,
    fn list_user_sessions(pool: &DbPool, user_id: i64) -> Result<Vec<Session>>);

// Ends one session if it belongs to the account. Returns whether a session was ended.
native_operation!(crate::backend::db::postgres::users::sessions::revoke_user_session,
    crate::backend::db::sqlite::users::sessions::revoke_user_session,
    fn revoke_user_session(pool: &DbPool, user_id: i64, session_id: &str) -> Result<bool>);

// Ends every session of the account except `keep`. Returns how many were ended.
native_operation!(crate::backend::db::postgres::users::sessions::revoke_other_user_sessions,
    crate::backend::db::sqlite::users::sessions::revoke_other_user_sessions,
    fn revoke_other_user_sessions(pool: &DbPool, user_id: i64, keep: &str) -> Result<u64>);

native_operation!(crate::backend::db::postgres::users::sessions::create_browser_session,
    crate::backend::db::sqlite::users::sessions::create_browser_session,
    fn create_browser_session(
    pool: &DbPool,
    id: &str,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<BrowserSession>);

native_operation!(crate::backend::db::postgres::users::sessions::get_or_create_csrf_token,
    crate::backend::db::sqlite::users::sessions::get_or_create_csrf_token,
    fn get_or_create_csrf_token(
    pool: &DbPool,
    id: &str,
    expected_user: Option<i64>,
    candidate: &str,
    ttl_seconds: i64,
) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::users::sessions::remove_expired_identity_rows,
    crate::backend::db::sqlite::users::sessions::remove_expired_identity_rows,
    fn remove_expired_identity_rows(pool: &DbPool) -> Result<u64>);

native_operation!(crate::backend::db::postgres::users::sessions::keep_newest_anonymous_sessions,
    crate::backend::db::sqlite::users::sessions::keep_newest_anonymous_sessions,
    fn keep_newest_anonymous_sessions(pool: &DbPool, cap: i64) -> Result<u64>);
