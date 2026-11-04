use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Application {
    pub id: i64,
    pub owner_id: i64,
    pub client_id: String,
    pub secret_hash: String,
    pub name: String,
    pub homepage: String,
    pub redirect_uri: String,
    pub created_at: DateTime<Utc>,
}

pub const APPLICATION_PAGE_SIZE: i64 = 25;
pub(crate) const APPLICATIONS_SQL: &str = "SELECT name,client_id,redirect_uri FROM oauth_applications WHERE owner_id=$1 ORDER BY id LIMIT $2 OFFSET $3";

#[derive(sqlx::FromRow)]
pub struct ApplicationSummary {
    pub name: String,
    pub client_id: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Grant {
    pub hash: String,
    pub client_id: String,
    pub user_code: String,
    pub scopes: String,
    pub expires_at: i64,
    pub interval: i64,
    pub last_poll: i64,
    pub user_id: Option<i64>,
    pub session_id: Option<String>,
    pub denied: bool,
    pub kind: String,
    pub redirect_uri: String,
    pub state: String,
    pub challenge: String,
}

pub enum Exchange {
    Token(String),
    Pending,
    SlowDown(i64),
    Denied,
    Expired,
    Invalid,
}

native_operation!(crate::postgres::oauth_apps::register,
    crate::sqlite::oauth_apps::register,
    fn register(pool: &DbPool, actor: i64, session: &str, client_id: &str, secret_hash: &str, name: &str, homepage: &str, redirect_uri: &str) -> Result<Option<Application>>);
native_operation!(crate::postgres::oauth_apps::application,
    crate::sqlite::oauth_apps::application,
    fn application(pool: &DbPool, client_id: &str) -> Result<Option<Application>>);
native_operation!(crate::postgres::oauth_apps::applications,
    crate::sqlite::oauth_apps::applications,
    fn applications(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, offset: i64, limit: i64) -> Result<Option<Vec<ApplicationSummary>>>);
native_operation!(crate::postgres::oauth_apps::save_grant,
    crate::sqlite::oauth_apps::save_grant,
    fn save_grant(pool: &DbPool, grant: &Grant) -> Result<()>);
native_operation!(crate::postgres::oauth_apps::device_grant,
    crate::sqlite::oauth_apps::device_grant,
    fn device_grant(pool: &DbPool, code: &str) -> Result<Option<Grant>>);
native_operation!(crate::postgres::oauth_apps::approve,
    crate::sqlite::oauth_apps::approve,
    fn approve(pool: &DbPool, code: &str, actor: i64, session: &str, denied: bool) -> Result<bool>);
native_operation!(crate::postgres::oauth_apps::exchange,
    crate::sqlite::oauth_apps::exchange,
    fn exchange(pool: &DbPool, hash: &str, client_id: &str, kind: &str, redirect_uri: Option<&str>, verifier: &str, state: Option<&str>, value: &str) -> Result<Exchange>);
native_operation!(codeyard_db_core::queries::postgres::oauth_apps::token,
    crate::sqlite::oauth_apps::token,
    fn token(pool: &DbPool, id: i64) -> Result<Option<Token>>);
native_operation!(crate::postgres::oauth_apps::owned_token,
    crate::sqlite::oauth_apps::owned_token,
    fn owned_token(pool: &DbPool, client: &str, hash: &str) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::postgres::oauth_apps::reset_token,
    crate::sqlite::oauth_apps::reset_token,
    fn reset_token(pool: &DbPool, client: &str, hash: &str, new_hash: &str) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::postgres::oauth_apps::revoke,
    crate::sqlite::oauth_apps::revoke,
    fn revoke(pool: &DbPool, client: &str, hash: &str, authorization: bool) -> Result<()>);
