use super::{
    admission::{self, AdmissionError},
    credentials::{
        validate_account, validate_email, validate_new_account_name, validate_new_password,
        AccountInputError, PasswordAccount,
    },
    passwords::{self, PasswordError},
    sessions::{self, AccountAuthorityError},
    setup,
};
use crate::backend::db::{
    self,
    models::User,
    users::{
        signup_requests::ReviewError,
        transitions::{self, AccountAdmission, BanChange, EmailChange, RoleChange},
    },
    DbPool,
};
use crate::backend::email::links::{self, Purpose};

pub struct PasswordChange<'a> {
    pub current_password: &'a str,
    pub new_password: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PasswordChangeError {
    Authority(AccountAuthorityError),
    Input(AccountInputError),
    Admission(AdmissionError),
    Password(PasswordError),
    InvalidCredentials,
    UnchangedPassword,
    AccountUnavailable,
    CredentialsChanged,
}

pub async fn change_password(
    pool: &DbPool,
    session_id: Option<&str>,
    input: PasswordChange<'_>,
) -> Result<User, PasswordChangeError> {
    let user = sessions::password_change_account(pool, session_id)
        .await
        .map_err(PasswordChangeError::Authority)?;
    validate_new_password(input.new_password).map_err(PasswordChangeError::Input)?;
    let attempt = admission::admit_login(pool, None, Some(user.id), &user.username)
        .await
        .map_err(PasswordChangeError::Admission)?;
    if !passwords::verify_password(input.current_password, user.password_hash.as_deref())
        .await
        .map_err(PasswordChangeError::Password)?
        .valid
    {
        return Err(PasswordChangeError::InvalidCredentials);
    }
    admission::forgive_login(pool, &attempt).await;
    if user.must_change_password && input.current_password == input.new_password {
        return Err(PasswordChangeError::UnchangedPassword);
    }
    let expected = user
        .password_hash
        .as_deref()
        .ok_or(PasswordChangeError::InvalidCredentials)?;
    let hash = passwords::hash_password(input.new_password)
        .await
        .map_err(PasswordChangeError::Password)?;
    // Hash replacement, must-change clearance and every session's revocation are
    // one existing transaction. Failure leaves all three prior facts intact.
    transitions::compare_exchange_password(pool, user.id, expected, &hash, true, true, session_id)
        .await
        .map_err(|_| PasswordChangeError::AccountUnavailable)?
        .ok_or(PasswordChangeError::CredentialsChanged)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PasswordResetError {
    Input(AccountInputError),
    Password(PasswordError),
    /// The link is unknown, expired, already used, or no longer fits the account.
    InvalidLink,
    AccountUnavailable,
}

/// Sets a new password from the link in a reset email. The link works once. Like a password
/// change, it ends every session and access token of the account.
pub async fn reset_password(
    pool: &DbPool,
    token: &str,
    new_password: &str,
) -> Result<(), PasswordResetError> {
    validate_new_password(new_password).map_err(PasswordResetError::Input)?;
    // Hashing is costly, so it waits until the link is known to be good.
    let valid = links::is_valid(pool, Purpose::Reset, token)
        .await
        .map_err(|_| PasswordResetError::AccountUnavailable)?;
    if !valid {
        return Err(PasswordResetError::InvalidLink);
    }
    let hash = passwords::hash_password(new_password)
        .await
        .map_err(PasswordResetError::Password)?;
    let (user_id, _) = links::take(pool, Purpose::Reset, token)
        .await
        .map_err(|_| PasswordResetError::AccountUnavailable)?
        .ok_or(PasswordResetError::InvalidLink)?;
    let user = db::users::accounts::get_user_by_id(pool, user_id)
        .await
        .map_err(|_| PasswordResetError::AccountUnavailable)?
        .ok_or(PasswordResetError::InvalidLink)?;
    let expected = user
        .password_hash
        .as_deref()
        .ok_or(PasswordResetError::InvalidLink)?;
    transitions::compare_exchange_password(pool, user.id, expected, &hash, true, true, None)
        .await
        .map_err(|_| PasswordResetError::AccountUnavailable)?
        .map(|_| ())
        .ok_or(PasswordResetError::InvalidLink)
}

#[derive(Debug, PartialEq, Eq)]
pub enum EmailChangeError {
    Authority(AccountAuthorityError),
    InvalidEmail,
    Admission(AdmissionError),
    Password(PasswordError),
    InvalidCredentials,
    Taken,
    AccountUnavailable,
}

/// Changes the account's address after the current password is checked. The new address
/// is trimmed and must look like `name@host`; a wrong password counts against the account's
/// login budget.
pub async fn change_email(
    pool: &DbPool,
    session_id: Option<&str>,
    current_password: &str,
    email: &str,
) -> Result<User, EmailChangeError> {
    let user = sessions::password_change_account(pool, session_id)
        .await
        .map_err(EmailChangeError::Authority)?;
    let email = email.trim();
    let well_formed = email.split_once('@').is_some_and(|(name, host)| {
        !name.is_empty() && host.contains('.') && !email.contains(char::is_whitespace)
    });
    if !well_formed || validate_email(email).is_err() {
        return Err(EmailChangeError::InvalidEmail);
    }
    let attempt = admission::admit_login(pool, None, Some(user.id), &user.username)
        .await
        .map_err(EmailChangeError::Admission)?;
    if !passwords::verify_password(current_password, user.password_hash.as_deref())
        .await
        .map_err(EmailChangeError::Password)?
        .valid
    {
        return Err(EmailChangeError::InvalidCredentials);
    }
    admission::forgive_login(pool, &attempt).await;
    match transitions::change_email(
        pool,
        user.id,
        email,
        user.password_hash
            .as_deref()
            .ok_or(EmailChangeError::InvalidCredentials)?,
        session_id.ok_or(EmailChangeError::InvalidCredentials)?,
    )
    .await
    .map_err(|_| EmailChangeError::AccountUnavailable)?
    {
        EmailChange::Updated(user) => {
            // The transition clears the flag for a new address; a repeated one stays verified.
            if !user.email_verified {
                let _ = links::send_verification(pool, &user).await;
            }
            Ok(*user)
        }
        EmailChange::Taken => Err(EmailChangeError::Taken),
        EmailChange::Missing => Err(EmailChangeError::InvalidCredentials),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManagedAccountError {
    Authority(AccountAuthorityError),
    PrivateNodeRequired,
    Input(AccountInputError),
    Password(PasswordError),
    AccountUnavailable,
    Conflict,
    Denied,
}

/// Creates a temporary-password account with its requested role in one authorized transaction.
pub async fn create_managed_account(
    pool: &DbPool,
    administrator_session: Option<&str>,
    input: PasswordAccount<'_>,
    make_admin: bool,
) -> Result<User, ManagedAccountError> {
    let administrator_session = administrator_session.ok_or(ManagedAccountError::Authority(
        AccountAuthorityError::NotAuthenticated,
    ))?;
    let admin = sessions::administrator(pool, Some(administrator_session))
        .await
        .map_err(ManagedAccountError::Authority)?;
    let setup = setup::status(pool)
        .await
        .map_err(|_| ManagedAccountError::AccountUnavailable)?;
    if !setup.setup_completed || setup.node_mode != "private" {
        return Err(ManagedAccountError::PrivateNodeRequired);
    }
    validate_account(&input).map_err(ManagedAccountError::Input)?;
    validate_new_account_name(input.username).map_err(ManagedAccountError::Input)?;
    let hash = passwords::hash_password(input.password)
        .await
        .map_err(ManagedAccountError::Password)?;
    // The transaction revalidates the actor, original session and private mode,
    // and sets the temporary credential's must-change flag.
    let user = match transitions::register_password(
        pool,
        input.username,
        input.email.filter(|email| !email.trim().is_empty()),
        &hash,
        Some((admin.id, administrator_session, make_admin)),
    )
    .await
    .map_err(|_| ManagedAccountError::AccountUnavailable)?
    {
        AccountAdmission::User(user) => *user,
        AccountAdmission::Conflict => return Err(ManagedAccountError::Conflict),
        _ => return Err(ManagedAccountError::Denied),
    };
    let _ = links::send_verification(pool, &user).await;
    Ok(user)
}

pub enum SignupDecision {
    Approve,
    Reject,
}

pub enum SignupReviewOutcome {
    Approved(Box<User>),
    Rejected,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SignupReviewError {
    Authority(AccountAuthorityError),
    PrivateNodeRequired,
    InvalidDecision,
    Denied,
    Missing,
    Processed,
    Conflict,
    AccountUnavailable,
}

fn review_failure(error: anyhow::Error) -> SignupReviewError {
    match error.downcast_ref::<ReviewError>() {
        Some(ReviewError::Denied) => SignupReviewError::Denied,
        Some(ReviewError::Missing) => SignupReviewError::Missing,
        Some(ReviewError::Processed) => SignupReviewError::Processed,
        Some(ReviewError::Conflict) => SignupReviewError::Conflict,
        None => SignupReviewError::AccountUnavailable,
    }
}

pub async fn review_signup(
    pool: &DbPool,
    administrator_session: Option<&str>,
    request_id: i64,
    decision: Option<SignupDecision>,
) -> Result<SignupReviewOutcome, SignupReviewError> {
    let admin = sessions::administrator(pool, administrator_session)
        .await
        .map_err(SignupReviewError::Authority)?;
    let setup = setup::status(pool)
        .await
        .map_err(|_| SignupReviewError::AccountUnavailable)?;
    if !setup.setup_completed || setup.node_mode != "private" {
        return Err(SignupReviewError::PrivateNodeRequired);
    }
    let decision = decision.ok_or(SignupReviewError::InvalidDecision)?;
    // These named transactions also revalidate reviewer authority after locking.
    match decision {
        SignupDecision::Approve => {
            let user = db::users::signup_requests::approve_signup_request(
                pool,
                request_id,
                (
                    admin.id,
                    administrator_session.unwrap_or_default().to_owned(),
                ),
            )
            .await
            .map_err(review_failure)?;
            crate::backend::email::signup_decision(
                pool,
                user.email.as_deref(),
                &user.username,
                true,
            )
            .await;
            let _ = links::send_verification(pool, &user).await;
            Ok(SignupReviewOutcome::Approved(Box::new(user)))
        }
        SignupDecision::Reject => {
            let (username, email) = db::users::signup_requests::reject_signup_request(
                pool,
                request_id,
                (
                    admin.id,
                    administrator_session.unwrap_or_default().to_owned(),
                ),
            )
            .await
            .map_err(review_failure)?;
            crate::backend::email::signup_decision(pool, email.as_deref(), &username, false).await;
            Ok(SignupReviewOutcome::Rejected)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BanError {
    Authority(AccountAuthorityError),
    AccountUnavailable,
    Denied,
    Missing,
    SelfBan,
    LastAdmin,
}

pub async fn set_ban(
    pool: &DbPool,
    administrator_session: Option<&str>,
    target: i64,
    banned: bool,
) -> Result<User, BanError> {
    let admin = sessions::administrator(pool, administrator_session)
        .await
        .map_err(BanError::Authority)?;
    match transitions::change_ban(
        pool,
        (
            admin.id,
            administrator_session.unwrap_or_default().to_owned(),
        ),
        target,
        banned,
    )
    .await
    .map_err(|_| BanError::AccountUnavailable)?
    {
        BanChange::Updated(user) => Ok(*user),
        BanChange::Denied => Err(BanError::Denied),
        BanChange::Missing => Err(BanError::Missing),
        BanChange::SelfBan => Err(BanError::SelfBan),
        BanChange::LastAdmin => Err(BanError::LastAdmin),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RoleError {
    Authority(AccountAuthorityError),
    AccountUnavailable,
    Denied,
    Missing,
    SelfChange,
}

pub async fn set_admin_role(
    pool: &DbPool,
    administrator_session: Option<&str>,
    target: i64,
    admin: bool,
) -> Result<User, RoleError> {
    let actor = sessions::administrator(pool, administrator_session)
        .await
        .map_err(RoleError::Authority)?;
    match transitions::change_admin_role(
        pool,
        (
            actor.id,
            administrator_session.unwrap_or_default().to_owned(),
        ),
        target,
        admin,
    )
    .await
    .map_err(|_| RoleError::AccountUnavailable)?
    {
        RoleChange::Updated(user) => Ok(*user),
        RoleChange::Denied => Err(RoleError::Denied),
        RoleChange::Missing => Err(RoleError::Missing),
        RoleChange::SelfChange => Err(RoleError::SelfChange),
    }
}
