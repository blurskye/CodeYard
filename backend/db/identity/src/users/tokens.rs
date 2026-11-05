use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(crate::postgres::users::tokens::create_token,
    crate::sqlite::users::tokens::create_token,
    fn create_token(pool: &DbPool, user_id: i64, session_id: &str, name: &str, hash: &str,
        scope: &str, expires_at: Option<DateTime<Utc>>) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::postgres::users::tokens::list_tokens,
    crate::sqlite::users::tokens::list_tokens,
    fn list_tokens(pool: &DbPool, user_id: i64, session_id: &str, before: i64) -> Result<Vec<PersonalAccessToken>>);
native_operation!(crate::postgres::users::tokens::delete_token,
    crate::sqlite::users::tokens::delete_token,
    /// Deletes only while the browser session that asked is still signed in.
    fn delete_token(pool: &DbPool, user_id: i64, session_id: &str, token_id: i64) -> Result<bool>);
/// Native rows remain the home cell's credential store.
pub async fn find_native_token(pool: &DbPool, hash: &str) -> Result<Option<PersonalAccessToken>> {
    match pool {
        DbPool::Postgres(pool) => crate::postgres::users::tokens::find_token(pool, hash).await,
        DbPool::Sqlite(pool) => crate::sqlite::users::tokens::find_token(pool, hash).await,
    }
}
pub async fn find_token(pool: &DbPool, hash: &str) -> Result<Option<PersonalAccessToken>> {
    if let Some(token) = find_native_token(pool, hash).await? {
        return Ok(Some(token));
    }
    Ok(codeyard_db_core::cells::authorization::lookup(
        pool,
        &codeyard_db_core::cells::authorization::Query {
            kind: "token",
            id: None,
            hash: Some(hash),
            write: false,
        },
    )
    .await?
    .and_then(|credential| credential.token()))
}
native_operation!(crate::postgres::users::tokens::token_hash,
    crate::sqlite::users::tokens::token_hash,
    /// The stored hash of `user_id`'s token, so a background job can act with the credential it was given.
    fn token_hash(pool: &DbPool, token_id: i64, user_id: i64) -> Result<Option<String>>);
pub async fn token_user(pool: &DbPool, token_id: i64, write: bool) -> Result<Option<User>> {
    let native = match pool {
        DbPool::Postgres(pool) => {
            crate::postgres::users::tokens::token_user(pool, token_id, write).await?
        }
        DbPool::Sqlite(pool) => {
            crate::sqlite::users::tokens::token_user(pool, token_id, write).await?
        }
    };
    if native.is_some() {
        return Ok(native);
    }
    Ok(codeyard_db_core::cells::authorization::lookup(
        pool,
        &codeyard_db_core::cells::authorization::Query {
            kind: "token",
            id: Some(token_id),
            hash: None,
            write,
        },
    )
    .await?
    .map(|credential| credential.user))
}
native_operation!(crate::postgres::users::tokens::repository_write,
    crate::sqlite::users::tokens::repository_write,
    /// Narrows native write authority at a Git publication boundary.
    fn repository_write(pool: &DbPool, token: i64, actor: i64, repo: i64, permission: &str) -> Result<bool>);
native_operation!(crate::postgres::users::tokens::can_write_repository,
    crate::sqlite::users::tokens::can_write_repository,
    fn can_write_repository(pool: &DbPool, actor: i64, token: i64, repo: i64, permission: &str) -> Result<bool>);
native_operation!(crate::postgres::users::tokens::touch_token,
    crate::sqlite::users::tokens::touch_token,
    /// Records when and from which client address a token was last used.
    fn touch_token(pool: &DbPool, token_id: i64, address: Option<&str>) -> Result<()>);
