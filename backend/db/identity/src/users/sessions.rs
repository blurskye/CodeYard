use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::BrowserSession;
use codeyard_db_core::models::OAuthState;
use codeyard_db_core::models::Session;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(crate::postgres::users::sessions::create_session,
    crate::sqlite::users::sessions::create_session,
    fn create_session(
    pool: &DbPool,
    session_id: &str,
    user_id: i64,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<Session>);

native_operation!(crate::postgres::users::sessions::get_session,
    crate::sqlite::users::sessions::get_session,
    fn get_session(pool: &DbPool, session_id: &str) -> Result<Option<Session>>);

pub async fn get_session_account(pool: &DbPool, session_id: &str) -> Result<Option<User>> {
    if foreign_session(pool, session_id) {
        return Ok(
            codeyard_db_core::cells::authorization::session(pool, session_id)
                .await?
                .map(|credential| credential.user),
        );
    }
    match pool {
        DbPool::Postgres(pool) => {
            crate::postgres::users::sessions::get_session_account(pool, session_id).await
        }
        DbPool::Sqlite(pool) => {
            crate::sqlite::users::sessions::get_session_account(pool, session_id).await
        }
    }
}

pub async fn get_session_user(pool: &DbPool, session_id: &str) -> Result<Option<User>> {
    Ok(get_session_account(pool, session_id)
        .await?
        .filter(|user| !user.must_change_password))
}

native_operation!(crate::postgres::users::sessions::delete_session,
    crate::sqlite::users::sessions::delete_session,
    fn delete_session(pool: &DbPool, session_id: &str) -> Result<()>);

native_operation!(crate::postgres::users::sessions::save_oauth_state,
    crate::sqlite::users::sessions::save_oauth_state,
    fn save_oauth_state(
    pool: &DbPool,
    state: &str,
    pkce_verifier: &str,
    provider: &str,
    redirect_uri: Option<&str>,
) -> Result<()>);

native_operation!(crate::postgres::users::sessions::get_oauth_state,
    crate::sqlite::users::sessions::get_oauth_state,
    fn get_oauth_state(pool: &DbPool, state: &str) -> Result<Option<OAuthState>>);

native_operation!(crate::postgres::users::sessions::save_bound_oauth_state,
    crate::sqlite::users::sessions::save_bound_oauth_state,
    fn save_bound_oauth_state(
    pool: &DbPool,
    state: &str,
    verifier: &str,
    provider: &str,
    redirect: &str,
    binding: &str,
    return_to: &str,
) -> Result<()>);

native_operation!(crate::postgres::users::sessions::consume_bound_oauth_state,
    crate::sqlite::users::sessions::consume_bound_oauth_state,
    fn consume_bound_oauth_state(
    pool: &DbPool,
    state: &str,
    provider: &str,
    binding: &str,
    redirect: &str,
) -> Result<Option<OAuthState>>);

pub async fn get_browser_session(pool: &DbPool, id: &str) -> Result<Option<BrowserSession>> {
    if foreign_session(pool, id) {
        return Ok(codeyard_db_core::cells::authorization::session(pool, id)
            .await?
            .map(|credential| BrowserSession {
                id: id.to_owned(),
                user_id: Some(credential.user.id),
                expires_at: credential.deadline().wall(),
                created_at: credential.user.created_at,
            }));
    }
    match pool {
        DbPool::Postgres(pool) => {
            crate::postgres::users::sessions::get_browser_session(pool, id).await
        }
        DbPool::Sqlite(pool) => crate::sqlite::users::sessions::get_browser_session(pool, id).await,
    }
}

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
    OrganizationOidc {
        organization: &'a str,
        provider: &'a str,
        subject: &'a str,
    },
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
    pub fn password(self) -> Option<&'a str> {
        match self {
            Self::Password(password) | Self::SecondFactor { password, .. } => Some(password),
            Self::OAuth | Self::OrganizationOidc { .. } => None,
        }
    }
}

/// Metadata supplied by the admitting transport, never an account authority proof.
#[derive(Clone, Copy, Default)]
pub struct SessionClient<'a> {
    pub user_agent: Option<&'a str>,
    pub ip_address: Option<&'a str>,
}

impl<'a> From<Option<&'a str>> for SessionClient<'a> {
    fn from(user_agent: Option<&'a str>) -> Self {
        Self {
            user_agent,
            ip_address: None,
        }
    }
}

pub async fn rotate_authenticated_session<'a, 'c>(
    pool: &DbPool,
    old: Option<&str>,
    fresh: &str,
    user_id: i64,
    credential: impl Into<SessionCredential<'a>>,
    client: impl Into<SessionClient<'c>>,
    expires_at: DateTime<Utc>,
) -> Result<Option<BrowserSession>> {
    let credential = credential.into();
    let client = client.into();
    let address = client
        .ip_address
        .and_then(|address| address.parse::<std::net::IpAddr>().ok())
        .map(|address| address.to_string());
    match pool {
        DbPool::Postgres(pool) => {
            crate::postgres::users::sessions::rotate_authenticated_session(
                pool,
                old,
                fresh,
                user_id,
                credential,
                client.user_agent,
                address.as_deref(),
                expires_at,
            )
            .await
        }
        DbPool::Sqlite(pool) => {
            crate::sqlite::users::sessions::rotate_authenticated_session(
                pool,
                old,
                fresh,
                user_id,
                credential,
                client.user_agent,
                address.as_deref(),
                expires_at,
            )
            .await
        }
    }
}

// The account's sessions that have not expired.
#[derive(sqlx::FromRow)]
pub struct SessionPageRow {
    #[sqlx(flatten)]
    pub session: Session,
    pub ordinal: i64,
}

native_operation!(crate::postgres::users::sessions::session_page,
    crate::sqlite::users::sessions::session_page,
    fn session_page(pool: &DbPool, user: i64, current: &str, before: i64) -> Result<Vec<SessionPageRow>>);

native_operation!(crate::postgres::users::sessions::list_user_sessions,
    crate::sqlite::users::sessions::list_user_sessions,
    fn list_user_sessions(pool: &DbPool, user_id: i64) -> Result<Vec<Session>>);

// Ends one session if it belongs to the account. Returns whether a session was ended.
native_operation!(crate::postgres::users::sessions::revoke_user_session,
    crate::sqlite::users::sessions::revoke_user_session,
    fn revoke_user_session(pool: &DbPool, user_id: i64, session_id: &str, current: &str) -> Result<bool>);

// Ends every session of the account except `keep`. Returns how many were ended.
native_operation!(crate::postgres::users::sessions::revoke_other_user_sessions,
    crate::sqlite::users::sessions::revoke_other_user_sessions,
    fn revoke_other_user_sessions(pool: &DbPool, user_id: i64, keep: &str) -> Result<u64>);

native_operation!(crate::postgres::users::sessions::create_browser_session,
    crate::sqlite::users::sessions::create_browser_session,
    fn create_browser_session(
    pool: &DbPool,
    id: &str,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<BrowserSession>);

pub async fn get_or_create_csrf_token(
    pool: &DbPool,
    id: &str,
    expected_user: Option<i64>,
    candidate: &str,
    ttl_seconds: i64,
) -> Result<Option<String>> {
    if foreign_session(pool, id) {
        return Ok(codeyard_db_core::cells::authorization::session(pool, id)
            .await?
            .and_then(|credential| match credential.answer.authority {
                codeyard_db_core::cells::authorization::Authority::Session { csrf }
                    if expected_user == Some(credential.user.id) =>
                {
                    Some(csrf)
                }
                _ => None,
            }));
    }
    match pool {
        DbPool::Postgres(pool) => {
            crate::postgres::users::sessions::get_or_create_csrf_token(
                pool,
                id,
                expected_user,
                candidate,
                ttl_seconds,
            )
            .await
        }
        DbPool::Sqlite(pool) => {
            crate::sqlite::users::sessions::get_or_create_csrf_token(
                pool,
                id,
                expected_user,
                candidate,
                ttl_seconds,
            )
            .await
        }
    }
}

fn foreign_session(pool: &DbPool, value: &str) -> bool {
    codeyard_types::cells::Secret::cookie(value).is_ok_and(|secret| !pool.owns_prefix(secret.cell))
}

native_operation!(crate::postgres::users::sessions::remove_expired_identity_rows,
    crate::sqlite::users::sessions::remove_expired_identity_rows,
    fn remove_expired_identity_rows(pool: &DbPool) -> Result<u64>);

native_operation!(crate::postgres::users::sessions::keep_newest_anonymous_sessions,
    crate::sqlite::users::sessions::keep_newest_anonymous_sessions,
    fn keep_newest_anonymous_sessions(pool: &DbPool, cap: i64) -> Result<u64>);
