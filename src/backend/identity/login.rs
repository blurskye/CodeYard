use super::{
    admission::{self, AdmissionError},
    passwords::{self, PasswordError},
    sessions::{self, SessionContext, SessionIssueError},
    setup,
    two_factor::{self, TwoFactorError},
    AuthenticatedAccount,
};
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::{self, DbPool};

/// The result of the password step. An account with two-factor sign-in gets no session yet:
/// `pending` is the sealed token the second-factor page sends back with its code.
pub enum FirstFactor {
    Authenticated(Box<AuthenticatedAccount>),
    SecondFactor { pending: String },
}

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
    /// Two-factor sign-in needs the master key to seal its token and read the secret.
    TwoFactorUnavailable,
    /// The sealed token is missing, expired, forged or from another client.
    PendingLoginExpired,
    InvalidTwoFactorCode,
}

pub async fn password_login(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    credentials: PasswordCredentials<'_>,
    session: SessionContext<'_>,
) -> Result<FirstFactor, LoginError> {
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
    let user = user.ok_or(LoginError::InvalidCredentials)?;
    if user.is_banned {
        return Err(LoginError::Banned);
    }
    if two_factor::is_enabled(pool, user.id)
        .await
        .map_err(|_| LoginError::AccountUnavailable)?
    {
        let encryptor = encryptor.ok_or(LoginError::TwoFactorUnavailable)?;
        let pending = two_factor::seal_pending_login(
            encryptor,
            user.id,
            session.user_agent,
            two_factor::now_unix(),
        )
        .map_err(|_| LoginError::TwoFactorUnavailable)?;
        return Ok(FirstFactor::SecondFactor { pending });
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
    Ok(FirstFactor::Authenticated(Box::new(AuthenticatedAccount {
        user,
        session_id,
    })))
}

/// The second step: a current TOTP or recovery code for the account the sealed token names. The
/// account budget that the password step charged applies here too, so guesses stay bounded.
pub async fn second_factor_login(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    pending: &str,
    code: &str,
    session: SessionContext<'_>,
) -> Result<AuthenticatedAccount, LoginError> {
    let encryptor = encryptor.ok_or(LoginError::TwoFactorUnavailable)?;
    let now = two_factor::now_unix();
    let user_id = two_factor::open_pending_login(encryptor, pending, session.user_agent, now)
        .ok_or(LoginError::PendingLoginExpired)?;
    let user = db::users::accounts::get_user_by_id(pool, user_id)
        .await
        .map_err(|_| LoginError::AccountUnavailable)?
        .ok_or(LoginError::PendingLoginExpired)?;
    if user.is_banned {
        return Err(LoginError::Banned);
    }
    admission::admit_login(pool, Some(user.id), &user.username)
        .await
        .map_err(LoginError::Admission)?;
    let valid = two_factor::verify_sign_in_code(pool, encryptor, user.id, code, now)
        .await
        .map_err(|error| match error {
            TwoFactorError::KeyUnavailable => LoginError::TwoFactorUnavailable,
            _ => LoginError::AccountUnavailable,
        })?;
    if !valid {
        return Err(LoginError::InvalidTwoFactorCode);
    }
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
