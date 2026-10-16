use super::{
    admission::{self, AdmissionError},
    passwords::{self, PasswordError},
    sessions::{self, SessionContext, SessionIssueError},
    setup, AuthenticatedAccount,
};
use crate::backend::db::{self, users::transitions, DbPool};

pub struct PasswordCredentials<'a> {
    /// Username or email; admission is charged to the resolved canonical account.
    pub identity: &'a str,
    pub password: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    CredentialsBounds,
    SetupRequired,
    AccountUnavailable,
    Admission(AdmissionError),
    Password(PasswordError),
    InvalidCredentials,
    Banned,
    Session(SessionIssueError),
}

pub async fn password_login(
    pool: &DbPool,
    credentials: PasswordCredentials<'_>,
    session: SessionContext<'_>,
) -> Result<AuthenticatedAccount, LoginError> {
    if credentials.identity.len() > 1024
        || credentials.identity.chars().count() > 255
        || credentials.password.len() > passwords::MAX_PASSWORD_BYTES
    {
        return Err(LoginError::CredentialsBounds);
    }
    if !setup::status(pool)
        .await
        .map_err(|_| LoginError::AccountUnavailable)?
        .setup_completed
    {
        return Err(LoginError::SetupRequired);
    }
    let user = db::users::accounts::get_user_by_username(pool, credentials.identity)
        .await
        .map_err(|_| LoginError::AccountUnavailable)?;
    let user = match user {
        Some(user) => Some(user),
        None => db::users::accounts::get_user_by_email(pool, credentials.identity)
            .await
            .map_err(|_| LoginError::AccountUnavailable)?,
    };
    // Shared admission precedes every KDF, including missing/OAuth-only accounts.
    admission::admit_login(
        pool,
        user.as_ref().map(|user| user.id),
        credentials.identity,
    )
    .await
    .map_err(LoginError::Admission)?;
    let verification = passwords::verify_password(
        credentials.password,
        user.as_ref().and_then(|user| user.password_hash.as_deref()),
    )
    .await
    .map_err(LoginError::Password)?;
    if !verification.valid {
        return Err(LoginError::InvalidCredentials);
    }
    let mut user = user.ok_or(LoginError::InvalidCredentials)?;
    if user.is_banned {
        return Err(LoginError::Banned);
    }
    if verification.legacy {
        let old = user
            .password_hash
            .as_deref()
            .ok_or(LoginError::InvalidCredentials)?;
        let upgraded = passwords::hash_password(credentials.password)
            .await
            .map_err(LoginError::Password)?;
        let changed =
            transitions::compare_exchange_password(pool, user.id, old, &upgraded, false, false)
                .await
                .map_err(|_| LoginError::AccountUnavailable)?;
        // A concurrent reset/rehash wins; never overwrite or issue from stale credentials.
        if !changed {
            return Err(LoginError::InvalidCredentials);
        }
        user.password_hash = Some(upgraded);
    }
    // Password authentication deliberately permits the must-change account's
    // restricted session. Ordinary resource authority remains denied elsewhere.
    let session_id = sessions::issue_authenticated_session(
        pool,
        session,
        user.id,
        user.password_hash.as_deref(),
    )
    .await
    .map_err(LoginError::Session)?;
    Ok(AuthenticatedAccount { user, session_id })
}
