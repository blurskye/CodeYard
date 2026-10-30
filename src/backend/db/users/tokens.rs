use crate::backend::db::{
    models::{PersonalAccessToken, User},
    DbPool,
};
use anyhow::Result;
use chrono::{DateTime, Utc};

native_operation!(crate::backend::db::postgres::users::tokens::create_token,
    crate::backend::db::sqlite::users::tokens::create_token,
    fn create_token(pool: &DbPool, user_id: i64, session_id: &str, name: &str, hash: &str,
        scope: &str, expires_at: Option<DateTime<Utc>>) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::list_tokens,
    crate::backend::db::sqlite::users::tokens::list_tokens,
    fn list_tokens(pool: &DbPool, user_id: i64, before: i64) -> Result<Vec<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::delete_token,
    crate::backend::db::sqlite::users::tokens::delete_token,
    /// Deletes only while the browser session that asked is still signed in.
    fn delete_token(pool: &DbPool, user_id: i64, session_id: &str, token_id: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::tokens::find_token,
    crate::backend::db::sqlite::users::tokens::find_token,
    fn find_token(pool: &DbPool, hash: &str) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::token_user,
    crate::backend::db::sqlite::users::tokens::token_user,
    fn token_user(pool: &DbPool, token_id: i64, write: bool) -> Result<Option<User>>);
native_operation!(crate::backend::db::postgres::users::tokens::repository_write,
    crate::backend::db::sqlite::users::tokens::repository_write,
    /// Narrows native write authority at a Git publication boundary.
    fn repository_write(pool: &DbPool, token: i64, actor: i64, repo: i64, permission: &str) -> Result<bool>);
/// Read-only authorization transaction retained across the atomic Git ref update.
/// Dropping it releases the locks; it must be released before database follow-up writes.
pub enum PublicationGrant<'a> {
    Postgres(sqlx::Transaction<'a, sqlx::Postgres>),
    Sqlite(sqlx::Transaction<'a, sqlx::Sqlite>),
}

pub async fn publication_grant(
    pool: &DbPool,
    token: i64,
    actor: i64,
    repo: i64,
    workflows: bool,
) -> Result<Option<PublicationGrant<'_>>> {
    match pool {
        DbPool::Postgres(pool) => crate::backend::db::postgres::users::tokens::publication_grant(pool, token, actor, repo, workflows)
            .await.map(|grant| grant.map(PublicationGrant::Postgres)),
        DbPool::Sqlite(pool) => crate::backend::db::sqlite::users::tokens::publication_grant(pool, token, actor, repo, workflows)
            .await.map(|grant| grant.map(PublicationGrant::Sqlite)),
    }
}

native_operation!(crate::backend::db::postgres::users::tokens::touch_token,
    crate::backend::db::sqlite::users::tokens::touch_token,
    /// Records when and from which client address a token was last used.
    fn touch_token(pool: &DbPool, token_id: i64, address: Option<&str>) -> Result<()>);
