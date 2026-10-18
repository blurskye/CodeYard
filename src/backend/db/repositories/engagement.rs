use crate::backend::db::models::{Repository, User};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::repositories::engagement::watch_repo,
    crate::backend::db::sqlite::repositories::engagement::watch_repo,
    fn watch_repo(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::engagement::unwatch_repo,
    crate::backend::db::sqlite::repositories::engagement::unwatch_repo,
    fn unwatch_repo(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::engagement::is_repo_watched,
    crate::backend::db::sqlite::repositories::engagement::is_repo_watched,
    fn is_repo_watched(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::engagement::set_watch_level,
    crate::backend::db::sqlite::repositories::engagement::set_watch_level,
    fn set_watch_level(pool: &DbPool, user_id: i64, repo_id: i64, level: &str, events: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::engagement::get_watch_level,
    crate::backend::db::sqlite::repositories::engagement::get_watch_level,
    fn get_watch_level(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<Option<(String, String)>>);

native_operation!(crate::backend::db::postgres::repositories::engagement::list_event_watchers,
    crate::backend::db::sqlite::repositories::engagement::list_event_watchers,
    fn list_event_watchers(pool: &DbPool, repo_id: i64, event: &str, after_user_id: i64, limit: i64) -> Result<Vec<i64>>);

native_operation!(crate::backend::db::postgres::repositories::engagement::star_repo,
    crate::backend::db::sqlite::repositories::engagement::star_repo,
    fn star_repo(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::engagement::unstar_repo,
    crate::backend::db::sqlite::repositories::engagement::unstar_repo,
    fn unstar_repo(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::engagement::is_repo_starred,
    crate::backend::db::sqlite::repositories::engagement::is_repo_starred,
    fn is_repo_starred(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::engagement::list_repo_stargazers,
    crate::backend::db::sqlite::repositories::engagement::list_repo_stargazers,
    fn list_repo_stargazers(pool: &DbPool, repo_id: i64, limit: i64, offset: i64) -> Result<Vec<User>>);

/// The order of a starred list. The page is taken after this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StarSort {
    /// The latest star first.
    #[default]
    Recent,
    /// The most stars first, then the latest star.
    Stars,
    /// By repository name, ignoring case.
    Name,
}

native_operation!(crate::backend::db::postgres::repositories::engagement::list_starred_repositories,
    crate::backend::db::sqlite::repositories::engagement::list_starred_repositories,
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

native_operation!(crate::backend::db::postgres::repositories::engagement::count_starred_repositories,
    crate::backend::db::sqlite::repositories::engagement::count_starred_repositories,
    /// How many repositories `list_starred_repositories` lists for the same arguments.
    fn count_starred_repositories(pool: &DbPool, starred_by: i64, viewer: Option<i64>, text: &str) -> Result<i64>);
