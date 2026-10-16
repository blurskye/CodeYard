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
    fn list_tokens(pool: &DbPool, user_id: i64) -> Result<Vec<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::delete_token,
    crate::backend::db::sqlite::users::tokens::delete_token,
    fn delete_token(pool: &DbPool, user_id: i64, token_id: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::tokens::find_token,
    crate::backend::db::sqlite::users::tokens::find_token,
    fn find_token(pool: &DbPool, hash: &str) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::users::tokens::token_user,
    crate::backend::db::sqlite::users::tokens::token_user,
    fn token_user(pool: &DbPool, token_id: i64, write: bool) -> Result<Option<User>>);
