use super::{
    admission::{self, AdmissionError},
    credentials::{
        validate_account, validate_email, validate_new_account_name, AccountInputError,
        PasswordAccount,
    },
    passwords::{self, PasswordError},
    sessions::{self, SessionContext, SessionIssueError},
    AuthenticatedAccount,
};
use crate::backend::db::{
    self,
    users::transitions::{self, SetupClaim},
    DbPool,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub struct SetupStatus {
    pub setup_completed: bool,
    pub node_mode: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetupStatusUnavailable;

pub async fn status(pool: &DbPool) -> Result<SetupStatus, SetupStatusUnavailable> {
    let config = db::node_config::get_node_configuration(pool)
        .await
        .map_err(|_| SetupStatusUnavailable)?;
    Ok(match config {
        Some(config) => SetupStatus {
            setup_completed: config.setup_completed,
            node_mode: config.node_mode,
        },
        None => SetupStatus {
            setup_completed: false,
            node_mode: "public".to_owned(),
        },
    })
}

pub struct SetupInput<'a> {
    pub account: PasswordAccount<'a>,
    pub node_mode: &'a str,
    pub admin_secret: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupError {
    Input(AccountInputError),
    InputBounds,
    InvalidNodeMode,
    InvalidAdminSecret,
    Admission(AdmissionError),
    Password(PasswordError),
    AccountUnavailable,
    AlreadyCompleted,
    ExistingAccounts,
    Session(SessionIssueError),
}

fn node_mode(mode: &str) -> Option<&'static str> {
    if mode.len() > 32 {
        return None;
    }
    match mode.trim().to_ascii_lowercase().as_str() {
        "public" => Some("public"),
        "private" => Some("private"),
        _ => None,
    }
}

pub async fn complete_setup(
    pool: &DbPool,
    input: SetupInput<'_>,
    expected_admin_secret: Option<&str>,
    session: SessionContext<'_>,
) -> Result<AuthenticatedAccount, SetupError> {
    // Keep the existing setup-specific bounds outcome (including email).
    let account = PasswordAccount {
        username: input.account.username,
        email: None,
        password: input.account.password,
    };
    validate_account(&account).map_err(SetupError::Input)?;
    if input
        .account
        .email
        .is_some_and(|email| validate_email(email).is_err())
        || input.admin_secret.is_some_and(|secret| secret.len() > 4096)
    {
        return Err(SetupError::InputBounds);
    }
    let mode = node_mode(input.node_mode).ok_or(SetupError::InvalidNodeMode)?;
    let existing = db::users::accounts::get_user_by_username(pool, input.account.username)
        .await
        .map_err(|_| SetupError::AccountUnavailable)?;
    // Even installations with no configured bootstrap secret share admission. It is charged
    // before the secret is compared, so guesses at the secret are bounded.
    admission::admit_login(
        pool,
        existing.as_ref().map(|user| user.id),
        input.account.username,
    )
    .await
    .map_err(SetupError::Admission)?;
    if let Some(expected) = expected_admin_secret {
        let supplied = input.admin_secret.map(str::trim).unwrap_or("");
        if !bool::from(
            Sha256::digest(expected.as_bytes()).ct_eq(&Sha256::digest(supplied.as_bytes())),
        ) {
            return Err(SetupError::InvalidAdminSecret);
        }
    }
    // Recovery of an existing administrator keeps its name; only a new account is checked.
    if existing.is_none() {
        validate_new_account_name(input.account.username).map_err(SetupError::Input)?;
    }
    let recovery = if let Some(user) = &existing {
        let verified =
            passwords::verify_password(input.account.password, user.password_hash.as_deref())
                .await
                .map_err(SetupError::Password)?;
        if verified.valid && user.is_admin && !user.is_banned {
            user.password_hash.as_deref().map(|hash| (user.id, hash))
        } else {
            None
        }
    } else {
        None
    };
    let hash = passwords::hash_password(input.account.password)
        .await
        .map_err(SetupError::Password)?;
    let user = match transitions::complete_setup(
        pool,
        input.account.username,
        input.account.email.filter(|email| !email.trim().is_empty()),
        &hash,
        mode,
        recovery,
    )
    .await
    .map_err(|_| SetupError::AccountUnavailable)?
    {
        SetupClaim::Completed(user) => *user,
        SetupClaim::AlreadyCompleted => return Err(SetupError::AlreadyCompleted),
        SetupClaim::ExistingAccounts => return Err(SetupError::ExistingAccounts),
    };
    // Account/config commit precedes issuance. Failure cannot reopen setup; the
    // completed administrator retries ordinary sign-in, not bootstrap adoption.
    let session_id =
        sessions::issue_authenticated_session(pool, session, user.id, Some(hash.as_str()))
            .await
            .map_err(SetupError::Session)?;
    Ok(AuthenticatedAccount { user, session_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_mode_is_normalized_and_bounded() {
        assert_eq!(node_mode(" PRIVATE "), Some("private"));
        assert_eq!(node_mode("Public"), Some("public"));
        assert_eq!(node_mode("unknown"), None);
        assert_eq!(node_mode(&" ".repeat(33)), None);
    }
}
