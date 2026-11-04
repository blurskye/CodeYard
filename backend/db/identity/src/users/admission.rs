use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

/// Failed attempts one client address may make in a window.
pub(crate) const CLIENT_LIMIT: i32 = 30;
/// Failed attempts one account may take in a window.
pub(crate) const ACCOUNT_LIMIT: i32 = 10;
pub(crate) const WINDOW_SECONDS: i64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub struct Receipt {
    pub client: Option<(String, chrono::DateTime<chrono::Utc>)>,
    pub account: (String, chrono::DateTime<chrono::Utc>),
}

native_operation!(crate::postgres::users::admission::admit_password_attempt,
    crate::sqlite::users::admission::admit_password_attempt,
    fn admit_password_attempt(pool: &DbPool, client_key: Option<&str>, account_key: &str) -> Result<Option<Receipt>>);

native_operation!(crate::postgres::users::admission::forgive_password_attempt,
    crate::sqlite::users::admission::forgive_password_attempt,
    fn forgive_password_attempt(pool: &DbPool, receipt: &Receipt) -> Result<()>);
