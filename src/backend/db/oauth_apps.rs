//! OAuth applications and single-use device and web authorizations.
use crate::backend::db::{models::PersonalAccessToken, DbPool};
use anyhow::Result;
use chrono::{DateTime, Utc};

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

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Token {
    pub token_id: i64,
    pub client_id: String,
    pub scopes: String,
}

/// Native SQL fragments only: client authentication and token use share issuer liveness.
pub(crate) fn client_condition(client_sql: &str) -> String {
    format!("EXISTS(SELECT 1 FROM oauth_applications client JOIN users owner ON owner.id=client.owner_id
        WHERE client.client_id={client_sql} AND NOT owner.is_banned AND NOT owner.must_change_password
        AND (NOT client.is_github_app OR EXISTS(
            SELECT 1 FROM github_apps app JOIN users bot ON bot.id=app.bot_id JOIN users app_owner ON app_owner.id=app.owner_id
            WHERE app.client_id=client.client_id AND NOT bot.is_banned AND NOT bot.must_change_password
            AND NOT app_owner.is_banned AND NOT app_owner.must_change_password)))")
}

pub(crate) fn token_issuer_condition(token_sql: &str, cli_sql: &str) -> String {
    format!("NOT EXISTS(SELECT 1 FROM oauth_app_tokens issued WHERE issued.token_id={token_sql}.id
        AND issued.client_id<>{cli_sql} AND NOT ({}))", client_condition("issued.client_id"))
}

pub enum Exchange {
    Token(String),
    Pending,
    SlowDown(i64),
    Denied,
    Expired,
    Invalid,
}

native_operation!(crate::backend::db::postgres::oauth_apps::register,
    crate::backend::db::sqlite::oauth_apps::register,
    #[expect(clippy::too_many_arguments, reason = "Registration stores the explicit validated application fields.")]
    fn register(pool: &DbPool, actor: i64, session: &str, client_id: &str, secret_hash: &str, name: &str, homepage: &str, redirect_uri: &str) -> Result<Option<Application>>);
native_operation!(crate::backend::db::postgres::oauth_apps::application,
    crate::backend::db::sqlite::oauth_apps::application,
    fn application(pool: &DbPool, client_id: &str) -> Result<Option<Application>>);
native_operation!(crate::backend::db::postgres::oauth_apps::applications,
    crate::backend::db::sqlite::oauth_apps::applications,
    fn applications(pool: &DbPool, actor: i64, session: &str) -> Result<Vec<Application>>);
native_operation!(crate::backend::db::postgres::oauth_apps::save_grant,
    crate::backend::db::sqlite::oauth_apps::save_grant,
    fn save_grant(pool: &DbPool, grant: &Grant) -> Result<()>);
native_operation!(crate::backend::db::postgres::oauth_apps::device_grant,
    crate::backend::db::sqlite::oauth_apps::device_grant,
    fn device_grant(pool: &DbPool, code: &str) -> Result<Option<Grant>>);
native_operation!(crate::backend::db::postgres::oauth_apps::approve,
    crate::backend::db::sqlite::oauth_apps::approve,
    fn approve(pool: &DbPool, code: &str, actor: i64, session: &str, denied: bool) -> Result<bool>);
native_operation!(crate::backend::db::postgres::oauth_apps::exchange,
    crate::backend::db::sqlite::oauth_apps::exchange,
    #[expect(clippy::too_many_arguments, reason = "OAuth exchange binds each credential to the original authorization request.")]
    fn exchange(pool: &DbPool, hash: &str, client_id: &str, kind: &str, redirect_uri: Option<&str>, verifier: &str, state: Option<&str>, value: &str) -> Result<Exchange>);
native_operation!(crate::backend::db::postgres::oauth_apps::token,
    crate::backend::db::sqlite::oauth_apps::token,
    fn token(pool: &DbPool, id: i64) -> Result<Option<Token>>);
native_operation!(crate::backend::db::postgres::oauth_apps::owned_token,
    crate::backend::db::sqlite::oauth_apps::owned_token,
    fn owned_token(pool: &DbPool, client: &str, hash: &str) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::oauth_apps::reset_token,
    crate::backend::db::sqlite::oauth_apps::reset_token,
    fn reset_token(pool: &DbPool, client: &str, hash: &str, new_hash: &str) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::oauth_apps::revoke,
    crate::backend::db::sqlite::oauth_apps::revoke,
    fn revoke(pool: &DbPool, client: &str, hash: &str, authorization: bool) -> Result<()>);
