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
    let attempt = admission::admit_login(
        pool,
        session.client,
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
    admission::forgive_login(pool, &attempt).await;
    let mut user = user.ok_or(LoginError::InvalidCredentials)?;
    if user.is_banned {
        return Err(LoginError::Banned);
    }
    if verification.outdated {
        upgrade_hash(pool, &mut user, credentials.password).await;
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
            user.password_hash.as_deref(),
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

/// Stores the password again at the current cost. This is best effort: a failure is logged and
/// the old hash keeps working. If another login stored a new hash first, the session is issued
/// against that hash, as long as it accepts the same password.
async fn upgrade_hash(pool: &DbPool, user: &mut db::models::User, password: &str) {
    let Some(old) = user.password_hash.clone() else {
        return;
    };
    let new = match passwords::hash_password(password).await {
        Ok(new) => new,
        Err(error) => {
            tracing::warn!(?error, "could not rehash a password at the current cost");
            return;
        }
    };
    match db::users::accounts::replace_password_hash(pool, user.id, &old, &new).await {
        Ok(true) => user.password_hash = Some(new),
        Ok(false) => adopt_current_hash(pool, user, password).await,
        Err(error) => tracing::warn!(?error, "could not store a rehashed password"),
    }
}

/// Picks up the stored hash after a lost rehash race, but only if it still accepts the password;
/// a real password change in between keeps the old hash, so the session is refused.
async fn adopt_current_hash(pool: &DbPool, user: &mut db::models::User, password: &str) {
    let Ok(Some(current)) = db::users::accounts::get_user_by_id(pool, user.id).await else {
        return;
    };
    if let Ok(check) = passwords::verify_password(password, current.password_hash.as_deref()).await
    {
        if check.valid {
            user.password_hash = current.password_hash;
        }
    }
}

/// The second step: a current TOTP or recovery code for the account the sealed token names. Wrong
/// codes count against the same budgets as wrong passwords, so guesses stay bounded.
pub async fn second_factor_login(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    pending: &str,
    code: &str,
    session: SessionContext<'_>,
) -> Result<AuthenticatedAccount, LoginError> {
    let encryptor = encryptor.ok_or(LoginError::TwoFactorUnavailable)?;
    let now = two_factor::now_unix();
    let (user_id, sealed_password) =
        two_factor::open_pending_login(encryptor, pending, session.user_agent, now)
            .ok_or(LoginError::PendingLoginExpired)?;
    let user = db::users::accounts::get_user_by_id(pool, user_id)
        .await
        .map_err(|_| LoginError::AccountUnavailable)?
        .ok_or(LoginError::PendingLoginExpired)?;
    // A password change ends the sign-in that started with the old password.
    if two_factor::password_digest(user.password_hash.as_deref()) != sealed_password {
        return Err(LoginError::PendingLoginExpired);
    }
    if user.is_banned {
        return Err(LoginError::Banned);
    }
    let attempt = admission::admit_login(pool, session.client, Some(user.id), &user.username)
        .await
        .map_err(LoginError::Admission)?;
    let normalized = two_factor::normalize_code(code);
    let recovery = (!normalized.is_empty()
        && !(normalized.len() == 6 && normalized.bytes().all(|b| b.is_ascii_digit())))
    .then(|| two_factor::recovery_code_hash(&normalized));
    let totp = if recovery.is_none() {
        Some(
            two_factor::verify_totp_code(pool, encryptor, user.id, code, now)
                .await
                .map_err(|error| match error {
                    TwoFactorError::KeyUnavailable => LoginError::TwoFactorUnavailable,
                    _ => LoginError::AccountUnavailable,
                })?
                .ok_or(LoginError::InvalidTwoFactorCode)?,
        )
    } else {
        None
    };
    let credential = db::users::sessions::SessionCredential::SecondFactor {
        password: user
            .password_hash
            .as_deref()
            .ok_or(LoginError::PendingLoginExpired)?,
        recovery_hash: recovery.as_deref(),
        totp: totp.as_ref().map(|(nonce, step)| (nonce.as_slice(), *step)),
    };
    let session_id = sessions::issue_authenticated_session(pool, session, user.id, credential)
        .await
        .map_err(|error| {
            if error == SessionIssueError::Denied {
                LoginError::InvalidTwoFactorCode
            } else {
                LoginError::Session(error)
            }
        })?;
    admission::forgive_login(pool, &attempt).await;
    Ok(AuthenticatedAccount { user, session_id })
}
