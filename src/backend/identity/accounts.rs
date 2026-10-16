use super::{
    admission::{self, AdmissionError},
    credentials::{
        validate_account, validate_new_account_name, validate_new_password, AccountInputError,
        PasswordAccount,
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
        transitions::{self, AccountAdmission, BanChange},
    },
    DbPool,
};

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
    admission::admit_login(pool, Some(user.id), &user.username)
        .await
        .map_err(PasswordChangeError::Admission)?;
    if !passwords::verify_password(input.current_password, user.password_hash.as_deref())
        .await
        .map_err(PasswordChangeError::Password)?
        .valid
    {
        return Err(PasswordChangeError::InvalidCredentials);
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
    if !transitions::compare_exchange_password(pool, user.id, expected, &hash, true, true)
        .await
        .map_err(|_| PasswordChangeError::AccountUnavailable)?
    {
        return Err(PasswordChangeError::CredentialsChanged);
    }
    db::users::accounts::get_user_by_id(pool, user.id)
        .await
        .map_err(|_| PasswordChangeError::AccountUnavailable)?
        .ok_or(PasswordChangeError::InvalidCredentials)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManagedAccountError {
    Authority(AccountAuthorityError),
    PrivateNodeRequired,
    Input(AccountInputError),
    Admission(AdmissionError),
    Password(PasswordError),
    AccountUnavailable,
    Conflict,
    Denied,
}

pub async fn create_managed_account(
    pool: &DbPool,
    administrator_session: Option<&str>,
    input: PasswordAccount<'_>,
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
    admission::admit_login(pool, Some(admin.id), &admin.username)
        .await
        .map_err(ManagedAccountError::Admission)?;
    let hash = passwords::hash_password(input.password)
        .await
        .map_err(ManagedAccountError::Password)?;
    // The transaction revalidates the actor, original session and private mode,
    // and sets the temporary credential's must-change flag.
    match transitions::register_password(
        pool,
        input.username,
        input.email,
        &hash,
        Some((admin.id, administrator_session)),
    )
    .await
    .map_err(|_| ManagedAccountError::AccountUnavailable)?
    {
        AccountAdmission::User(user) => Ok(*user),
        AccountAdmission::Conflict => Err(ManagedAccountError::Conflict),
        _ => Err(ManagedAccountError::Denied),
    }
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
    AccountUnavailable,
}

fn review_failure(error: anyhow::Error) -> SignupReviewError {
    match error.downcast_ref::<ReviewError>() {
        Some(ReviewError::Denied) => SignupReviewError::Denied,
        Some(ReviewError::Missing) => SignupReviewError::Missing,
        Some(ReviewError::Processed) => SignupReviewError::Processed,
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
            db::users::signup_requests::approve_signup_request(pool, request_id, admin.id)
                .await
                .map(|user| SignupReviewOutcome::Approved(Box::new(user)))
                .map_err(review_failure)
        }
        SignupDecision::Reject => {
            db::users::signup_requests::reject_signup_request(pool, request_id, admin.id)
                .await
                .map_err(review_failure)?;
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
    match transitions::change_ban(pool, admin.id, target, banned)
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
