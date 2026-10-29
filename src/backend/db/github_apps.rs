//! GitHub Apps, their account installations and bounded credentials.
use crate::backend::db::{
    models::{PersonalAccessToken, Repository},
    DbPool,
};
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct App {
    pub id: i64,
    pub owner_id: i64,
    pub bot_id: i64,
    pub is_public: bool,
    pub client_id: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub homepage: String,
    pub public_key: String,
    pub permissions: String,
    pub events: String,
    pub access_level: i16,
    pub created_at: DateTime<Utc>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Installation {
    pub id: i64,
    pub app_id: i64,
    pub user_id: Option<i64>,
    pub org_id: Option<i64>,
    pub all_repos: bool,
    pub suspended_at: Option<DateTime<Utc>>,
    pub suspended_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct InstallationRequest {
    pub id: i64,
    pub app_id: i64,
    pub org_id: i64,
    pub user_id: i64,
    pub created_at: DateTime<Utc>,
}

native_operation!(crate::backend::db::postgres::github_apps::request_installation, crate::backend::db::sqlite::github_apps::request_installation,
    fn request_installation(pool: &DbPool, actor: i64, session: &str, app: i64, org: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::github_apps::requests, crate::backend::db::sqlite::github_apps::requests,
    fn requests(pool: &DbPool, app: i64) -> Result<Vec<InstallationRequest>>);

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Token {
    pub token_id: i64,
    pub installation_id: i64,
    pub permissions: String,
    pub repositories: String,
    pub user_token: bool,
}

native_operation!(crate::backend::db::postgres::github_apps::register, crate::backend::db::sqlite::github_apps::register,
    fn register(pool: &DbPool, actor: i64, session: &str, app: &App, secret_hash: &str) -> Result<Option<App>>);
native_operation!(crate::backend::db::postgres::github_apps::client_active,crate::backend::db::sqlite::github_apps::client_active,
    fn client_active(pool:&DbPool,client:&str)->Result<bool>);
native_operation!(crate::backend::db::postgres::github_apps::app, crate::backend::db::sqlite::github_apps::app,
    fn app(pool: &DbPool, key: &str) -> Result<Option<App>>);
native_operation!(crate::backend::db::postgres::github_apps::app_by_id, crate::backend::db::sqlite::github_apps::app_by_id,
    fn app_by_id(pool: &DbPool, id: i64) -> Result<Option<App>>);
native_operation!(crate::backend::db::postgres::github_apps::apps, crate::backend::db::sqlite::github_apps::apps,
    fn apps(pool: &DbPool, actor: i64) -> Result<Vec<App>>);
native_operation!(crate::backend::db::postgres::github_apps::install, crate::backend::db::sqlite::github_apps::install,
    fn install(pool: &DbPool, actor: i64, session: &str, app: i64, user: Option<i64>, org: Option<i64>, repos: Option<&[i64]>) -> Result<Option<Installation>>);
/// Which installations a listing reads. Each `None` leaves its filter out; `actor` keeps the
/// installations that user reaches while their app's owner is active.
#[derive(Clone, Copy, Debug, Default)]
pub struct InstallationFilter {
    pub app: Option<i64>,
    pub actor: Option<i64>,
    pub org: Option<i64>,
    pub id: Option<i64>,
}
native_operation!(crate::backend::db::postgres::github_apps::installations, crate::backend::db::sqlite::github_apps::installations,
    /// A page of the installations a filter keeps, and the count of them all.
    fn installations(pool: &DbPool, filter: &InstallationFilter, offset: i64, limit: i64) -> Result<(i64, Vec<Installation>)>);
native_operation!(crate::backend::db::postgres::github_apps::account_installation, crate::backend::db::sqlite::github_apps::account_installation,
    fn account_installation(pool: &DbPool, app: i64, user: Option<i64>, org: Option<i64>) -> Result<Option<Installation>>);
native_operation!(crate::backend::db::postgres::github_apps::repository_installation, crate::backend::db::sqlite::github_apps::repository_installation,
    fn repository_installation(pool: &DbPool, app: i64, repo: i64, active: bool, actor: Option<i64>) -> Result<Option<Installation>>);
native_operation!(crate::backend::db::postgres::github_apps::installation, crate::backend::db::sqlite::github_apps::installation,
    fn installation(pool: &DbPool, id: i64) -> Result<Option<Installation>>);
native_operation!(crate::backend::db::postgres::github_apps::installation_repositories, crate::backend::db::sqlite::github_apps::installation_repositories,
    fn installation_repositories(pool: &DbPool, id: i64, viewer: Option<i64>, selected: &[i64], offset: i64, limit: i64) -> Result<(i64, Vec<Repository>)>);
#[derive(Debug, sqlx::FromRow)]
pub struct ManagedInstallation {
    pub id: i64,
    pub app_name: String,
    pub account_name: String,
    pub all_repos: bool,
}
native_operation!(crate::backend::db::postgres::github_apps::managed_installations,crate::backend::db::sqlite::github_apps::managed_installations,
    fn managed_installations(pool:&DbPool,actor:i64)->Result<Vec<ManagedInstallation>>);
native_operation!(crate::backend::db::postgres::github_apps::uninstall,crate::backend::db::sqlite::github_apps::uninstall,
    fn uninstall(pool:&DbPool,actor:i64,session:&str,id:i64)->Result<bool>);
native_operation!(crate::backend::db::postgres::github_apps::change_installation, crate::backend::db::sqlite::github_apps::change_installation,
    fn change_installation(pool: &DbPool, app: i64, id: i64, action: &str) -> Result<bool>);
/// What adding a repository to, or removing one from, a selected installation did.
#[derive(Debug, PartialEq, Eq)]
pub enum RepositoryChange {
    Changed,
    /// No such installation or repository for this actor.
    Missing,
    /// The installation covers all of its account's repositories.
    AllRepositories,
    /// Removing it would leave an organization installation without repositories.
    LastRepository,
}
native_operation!(crate::backend::db::postgres::github_apps::change_repository, crate::backend::db::sqlite::github_apps::change_repository,
    fn change_repository(pool: &DbPool, actor: impl Into<super::users::tokens::Actor>, id: i64, repo: i64, add: bool) -> Result<RepositoryChange>);
native_operation!(crate::backend::db::postgres::github_apps::issue, crate::backend::db::sqlite::github_apps::issue,
    #[expect(clippy::too_many_arguments, reason = "Credential issuance persists its explicit installation and permission bounds.")]
    fn issue(pool: &DbPool, app: i64, installation: i64, actor: Option<i64>, source: Option<&str>, hash: &str, permissions: &str, repos: &str, write: bool) -> Result<Option<PersonalAccessToken>>);
native_operation!(crate::backend::db::postgres::github_apps::token, crate::backend::db::sqlite::github_apps::token,
    fn token(pool: &DbPool, id: i64) -> Result<Option<Token>>);
native_operation!(crate::backend::db::postgres::github_apps::token_bounds, crate::backend::db::sqlite::github_apps::token_bounds,
    fn token_bounds(pool: &DbPool, id: i64) -> Result<Option<Token>>);
native_operation!(crate::backend::db::postgres::github_apps::revoke, crate::backend::db::sqlite::github_apps::revoke,
    fn revoke(pool: &DbPool, id: i64) -> Result<()>);
