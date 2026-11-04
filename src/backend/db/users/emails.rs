//! Secondary addresses share ownership checks with primary addresses, but not sign-in authority.
use crate::backend::db::users::accounts::AccountCredential;
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct EmailAddress {
    pub email: String,
    pub verified: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EmailRefusal {
    #[error("This email address is already registered")]
    Taken,
    #[error("The primary email address cannot be deleted")]
    Primary,
    #[error("At most 100 secondary email addresses may be added")]
    Limit,
}

native_operation!(crate::backend::db::postgres::users::emails::list_emails,
    crate::backend::db::sqlite::users::emails::list_emails,
    fn list_emails(pool: &DbPool, user: i64) -> Result<Vec<EmailAddress>>);
native_operation!(crate::backend::db::postgres::users::emails::list_account_emails,
    crate::backend::db::sqlite::users::emails::list_account_emails,
    /// The account's own settings listing, read while `credential` is still live.
    fn list_account_emails(pool: &DbPool, user: i64, credential: &AccountCredential<'_>) -> Result<Vec<EmailAddress>>);
native_operation!(crate::backend::db::postgres::users::emails::change_emails,
    crate::backend::db::sqlite::users::emails::change_emails,
    fn change_emails(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, emails: &[String], add: bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::users::emails::save_verification,
    crate::backend::db::sqlite::users::emails::save_verification,
    fn save_verification(pool: &DbPool, user: i64, email: &str, hash: &str, subject: &str, body_text: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::emails::verification_valid,
    crate::backend::db::sqlite::users::emails::verification_valid,
    fn verification_valid(pool: &DbPool, hash: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::emails::verify_email,
    crate::backend::db::sqlite::users::emails::verify_email,
    fn verify_email(pool: &DbPool, hash: &str) -> Result<bool>);
