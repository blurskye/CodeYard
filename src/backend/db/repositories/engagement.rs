use crate::backend::db::models::User;
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
