use anyhow::Result;
use chrono::{DateTime, Datelike, Utc};
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

/// Token timestamps must round-trip through SQLite's four-digit calendar representation.
pub(crate) fn supported_expiry(expiry: Option<DateTime<Utc>>) -> bool {
    expiry.is_none_or(|expiry| (1..=9999).contains(&expiry.year()))
}

native_operation!(crate::backend::db::postgres::users::tokens::create_token,
    crate::backend::db::sqlite::users::tokens::create_token,
    fn create_token(pool: &DbPool, user_id: i64, session_id: &str, name: &str, hash: &str,
        scope: &str, expires_at: Option<DateTime<Utc>>) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::list_tokens,
    crate::backend::db::sqlite::users::tokens::list_tokens,
    fn list_tokens(pool: &DbPool, user_id: i64, session_id: &str, before: i64) -> Result<Vec<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::delete_token,
    crate::backend::db::sqlite::users::tokens::delete_token,
    /// Deletes only while the browser session that asked is still signed in.
    fn delete_token(pool: &DbPool, user_id: i64, session_id: &str, token_id: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::tokens::find_token,
    crate::backend::db::sqlite::users::tokens::find_token,
    fn find_token(pool: &DbPool, hash: &str) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::token_hash,
    crate::backend::db::sqlite::users::tokens::token_hash,
    /// The stored hash of `user_id`'s token, so a background job can act with the credential it was given.
    fn token_hash(pool: &DbPool, token_id: i64, user_id: i64) -> Result<Option<String>>);
native_operation!(crate::backend::db::postgres::users::tokens::token_user,
    crate::backend::db::sqlite::users::tokens::token_user,
    fn token_user(pool: &DbPool, token_id: i64, write: bool) -> Result<Option<User>>);
native_operation!(crate::backend::db::postgres::users::tokens::repository_write,
    crate::backend::db::sqlite::users::tokens::repository_write,
    /// Narrows native write authority at a Git publication boundary.
    fn repository_write(pool: &DbPool, token: i64, actor: i64, repo: i64, permission: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::tokens::can_write_repository,
    crate::backend::db::sqlite::users::tokens::can_write_repository,
    fn can_write_repository(pool: &DbPool, actor: i64, token: i64, repo: i64, permission: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::tokens::touch_token,
    crate::backend::db::sqlite::users::tokens::touch_token,
    /// Records when and from which client address a token was last used.
    fn touch_token(pool: &DbPool, token_id: i64, address: Option<&str>) -> Result<()>);
