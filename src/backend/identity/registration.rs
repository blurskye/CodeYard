use super::{
    admission::{self, AdmissionError},
    credentials::{
        validate_account, validate_new_account_name, AccountInputError, PasswordAccount,
    },
    passwords::{self, PasswordError},
    sessions::{self, SessionContext, SessionIssueError},
    AuthenticatedAccount,
};
use crate::backend::db::{
    users::transitions::{self, AccountAdmission},
    DbPool,
};

pub enum RegistrationOutcome {
    Registered(Box<AuthenticatedAccount>),
    Requested,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegistrationError {
    Input(AccountInputError),
    Admission(AdmissionError),
    Password(PasswordError),
    AccountUnavailable,
    SetupRequired,
    Conflict,
    Denied,
    Session(SessionIssueError),
}

pub async fn register_password(
    pool: &DbPool,
    input: PasswordAccount<'_>,
    session: SessionContext<'_>,
) -> Result<RegistrationOutcome, RegistrationError> {
    validate_account(&input).map_err(RegistrationError::Input)?;
    validate_new_account_name(input.username).map_err(RegistrationError::Input)?;
    admission::admit_login(pool, None, input.username)
        .await
        .map_err(RegistrationError::Admission)?;
    let hash = passwords::hash_password(input.password)
        .await
        .map_err(RegistrationError::Password)?;
    let user = match transitions::register_password(pool, input.username, input.email, &hash, None)
        .await
        .map_err(|_| RegistrationError::AccountUnavailable)?
    {
        AccountAdmission::User(user) => *user,
        AccountAdmission::Requested => return Ok(RegistrationOutcome::Requested),
        AccountAdmission::SetupRequired => return Err(RegistrationError::SetupRequired),
        AccountAdmission::Conflict => return Err(RegistrationError::Conflict),
        _ => return Err(RegistrationError::Denied),
    };
    // The account is committed before issuance. A failed session never reopens
    // registration or changes a private-node request into an authenticated account.
    let session_id =
        sessions::issue_authenticated_session(pool, session, user.id, Some(hash.as_str()))
            .await
            .map_err(RegistrationError::Session)?;
    Ok(RegistrationOutcome::Registered(Box::new(
        AuthenticatedAccount { user, session_id },
    )))
}
