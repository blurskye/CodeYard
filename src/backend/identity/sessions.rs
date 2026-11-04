use chrono::{DateTime, Utc};
use codeyard_db_core::models::User;
use codeyard_db_core::DbPool;

/// The adapter distinguishes absent from invalid/ambiguous supplied identifiers.
/// No cookie/header parsing or transport-specific error lives here.
pub enum InitiatingSession<'a> {
    Missing,
    Supplied(&'a str),
    Invalid,
}

pub struct SessionContext<'a> {
    pub initiating: InitiatingSession<'a>,
    pub fresh_id: &'a str,
    pub user_agent: Option<&'a str>,
    /// The transport's client address, used for admission and retained with the session when
    /// it is an IP address.
    pub client: Option<&'a str>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionIssueError {
    InvalidInitiator,
    Unavailable,
    Denied,
}

pub async fn issue_authenticated_session<'a>(
    pool: &DbPool,
    context: SessionContext<'_>,
    user_id: i64,
    credential: impl Into<crate::backend::db::users::sessions::SessionCredential<'a>>,
) -> Result<String, SessionIssueError> {
    let old = match context.initiating {
        InitiatingSession::Missing => None,
        InitiatingSession::Supplied(id) => Some(id),
        InitiatingSession::Invalid => return Err(SessionIssueError::InvalidInitiator),
    };
    // One named transaction revalidates setup/account/credentials, rejects active
    // rebinds, and invalidates the initiating anonymous identity and its CSRF.
    let issued = crate::backend::db::users::sessions::rotate_authenticated_session(
        pool,
        old,
        context.fresh_id,
        user_id,
        credential,
        crate::backend::db::users::sessions::SessionClient {
            user_agent: context.user_agent,
            ip_address: context.client,
        },
        context.expires_at,
    )
    .await
    .map_err(|_| SessionIssueError::Unavailable)?
    .ok_or(SessionIssueError::Denied)?;
    Ok(issued.id)
}

#[derive(Debug, PartialEq, Eq)]
pub enum AccountAuthorityError {
    NotAuthenticated,
    SessionExpired,
    SessionUnavailable,
    PasswordChangeRequired,
    AdminRequired,
}

/// Resolve current durable authority inside the operation, not from an ingress
/// account snapshot. Temporary-password sessions are allowed only for this path.
pub(crate) async fn password_change_account(
    pool: &DbPool,
    session_id: Option<&str>,
) -> Result<User, AccountAuthorityError> {
    let id = session_id.ok_or(AccountAuthorityError::NotAuthenticated)?;
    crate::backend::db::users::sessions::get_session_account(pool, id)
        .await
        .map_err(|_| AccountAuthorityError::SessionUnavailable)?
        .ok_or(AccountAuthorityError::SessionExpired)
}

pub(crate) async fn administrator(
    pool: &DbPool,
    session_id: Option<&str>,
) -> Result<User, AccountAuthorityError> {
    let user = password_change_account(pool, session_id).await?;
    if user.must_change_password {
        return Err(AccountAuthorityError::PasswordChangeRequired);
    }
    if !user.is_admin {
        return Err(AccountAuthorityError::AdminRequired);
    }
    Ok(user)
}

#[derive(Debug, PartialEq, Eq)]
pub struct SessionStorageUnavailable;

pub async fn end_session(
    pool: &DbPool,
    session_id: Option<&str>,
) -> Result<(), SessionStorageUnavailable> {
    if let Some(id) = session_id {
        crate::backend::db::users::sessions::delete_session(pool, id)
            .await
            .map_err(|_| SessionStorageUnavailable)?;
    }
    Ok(())
}
