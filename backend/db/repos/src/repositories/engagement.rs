use anyhow::Result;
use codeyard_database::models::Repository;
use codeyard_database::models::User;
use codeyard_database::models::Watch;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

#[derive(sqlx::FromRow)]
pub struct StarHistoryWeek {
    pub week: chrono::NaiveDate,
    pub day0: i64,
    pub day1: i64,
    pub day2: i64,
    pub day3: i64,
    pub day4: i64,
    pub day5: i64,
    pub day6: i64,
}

native_operation!(crate::postgres::repositories::engagement::list_star_history,
    crate::sqlite::repositories::engagement::list_star_history,
    fn list_star_history(pool: &DbPool, repo_id: i64, limit: i64, offset: i64) -> Result<Vec<StarHistoryWeek>>);

native_operation!(crate::postgres::repositories::engagement::get_repo_watch,
    crate::sqlite::repositories::engagement::get_repo_watch,
    fn get_repo_watch(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<Option<Watch>>);
native_operation!(crate::postgres::repositories::engagement::list_repo_watchers,
    crate::sqlite::repositories::engagement::list_repo_watchers,
    fn list_repo_watchers(pool: &DbPool, repo_id: i64, limit: i64, offset: i64) -> Result<Vec<User>>);
native_operation!(crate::postgres::repositories::engagement::list_watched_repositories,
    crate::sqlite::repositories::engagement::list_watched_repositories,
    fn list_watched_repositories(pool: &DbPool, user_id: i64, viewer: Option<i64>, limit: i64, offset: i64) -> Result<Vec<Repository>>);

native_operation!(crate::postgres::repositories::engagement::watch_repo,
    crate::sqlite::repositories::engagement::watch_repo,
    fn watch_repo(pool: &DbPool, user_id: impl Into<codeyard_database::queries::credentials::Actor>, repo_id: i64) -> Result<bool>);

native_operation!(crate::postgres::repositories::engagement::unwatch_repo,
    crate::sqlite::repositories::engagement::unwatch_repo,
    fn unwatch_repo(pool: &DbPool, user_id: impl Into<codeyard_database::queries::credentials::Actor>, repo_id: i64) -> Result<bool>);

native_operation!(crate::postgres::repositories::engagement::is_repo_watched,
    crate::sqlite::repositories::engagement::is_repo_watched,
    fn is_repo_watched(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::postgres::repositories::engagement::set_watch_level,
    crate::sqlite::repositories::engagement::set_watch_level,
    fn set_watch_level(pool: &DbPool, user_id: impl Into<codeyard_database::queries::credentials::Actor>, repo_id: i64, level: &str, events: &str) -> Result<()>);

native_operation!(crate::postgres::repositories::engagement::get_watch_level,
    crate::sqlite::repositories::engagement::get_watch_level,
    fn get_watch_level(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<Option<(String, String)>>);

native_operation!(crate::postgres::repositories::engagement::list_event_watchers,
    crate::sqlite::repositories::engagement::list_event_watchers,
    fn list_event_watchers(pool: &DbPool, repo_id: i64, event: &str, after_user_id: i64, limit: i64) -> Result<Vec<i64>>);

native_operation!(crate::postgres::repositories::engagement::star_repo,
    crate::sqlite::repositories::engagement::star_repo,
    fn star_repo(pool: &DbPool, user_id: impl Into<codeyard_database::queries::credentials::Actor>, repo_id: i64) -> Result<bool>);

native_operation!(crate::postgres::repositories::engagement::unstar_repo,
    crate::sqlite::repositories::engagement::unstar_repo,
    fn unstar_repo(pool: &DbPool, user_id: impl Into<codeyard_database::queries::credentials::Actor>, repo_id: i64) -> Result<bool>);

native_operation!(crate::postgres::repositories::engagement::is_repo_starred,
    crate::sqlite::repositories::engagement::is_repo_starred,
    fn is_repo_starred(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::postgres::repositories::engagement::list_repo_stargazers,
    crate::sqlite::repositories::engagement::list_repo_stargazers,
    fn list_repo_stargazers(pool: &DbPool, repo_id: i64, limit: i64, offset: i64) -> Result<Vec<User>>);

native_operation!(crate::postgres::repositories::engagement::starred_at,
    crate::sqlite::repositories::engagement::starred_at,
    fn starred_at(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<Option<chrono::DateTime<chrono::Utc>>>);

/// The order of a starred list. The page is taken after this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StarSort {
    /// The latest star first.
    #[default]
    Recent,
    Oldest,
    Updated,
    UpdatedOldest,
    /// The most stars first, then the latest star.
    Stars,
    /// By repository name, ignoring case.
    Name,
}

native_operation!(crate::postgres::repositories::engagement::list_starred_repositories,
    crate::sqlite::repositories::engagement::list_starred_repositories,
    /// The repositories `starred_by` starred whose name or description contains `text`, that
    /// `viewer` can read. `None` is an anonymous visitor, who reads public repositories only.
    fn list_starred_repositories(
    pool: &DbPool,
    starred_by: i64,
    viewer: Option<i64>,
    text: &str,
    sort: StarSort,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>>);

native_operation!(crate::postgres::repositories::engagement::count_starred_repositories,
    crate::sqlite::repositories::engagement::count_starred_repositories,
    /// How many repositories `list_starred_repositories` lists for the same arguments.
    fn count_starred_repositories(pool: &DbPool, starred_by: i64, viewer: Option<i64>, text: &str) -> Result<i64>);
