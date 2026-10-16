//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{Issue, Repository, User};
use crate::backend::db::DbPool;
use anyhow::Result;

pub struct NewForkedRepository<'a> {
    pub owner_id: i64,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub is_private: bool,
    pub disk_path: &'a str,
    pub forked_from: i64,
    pub default_branch: &'a str,
}

native_operation!(crate::backend::db::postgres::repositories::discovery::create_forked_repository,
    crate::backend::db::sqlite::repositories::discovery::create_forked_repository,
    fn create_forked_repository(
    pool: &DbPool,
    input: NewForkedRepository<'_>,
) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::discovery::get_user_fork,
    crate::backend::db::sqlite::repositories::discovery::get_user_fork,
    fn get_user_fork(
    pool: &DbPool,
    user_id: i64,
    forked_from: i64,
) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::discovery::list_forks,
    crate::backend::db::sqlite::repositories::discovery::list_forks,
    fn list_forks(pool: &DbPool, repo_id: i64, limit: i64, offset: i64) -> Result<Vec<Repository>>);

/// What a repository search matches. `org:alice`, `user:alice` and `alice/proj` name an
/// owner; any other text matches the name, the description or the owner's name.
#[derive(Debug, PartialEq, Eq)]
pub enum RepositorySearch<'a> {
    Text(&'a str),
    /// The owner's name must equal `owner` exactly, and the repository name must contain `name`.
    Owned {
        owner: &'a str,
        name: &'a str,
    },
}

impl<'a> RepositorySearch<'a> {
    pub fn parse(query: &'a str) -> Self {
        let query = query.trim();
        if let Some((qualifier, owner)) = query.split_once(':') {
            if qualifier.eq_ignore_ascii_case("org") || qualifier.eq_ignore_ascii_case("user") {
                return Self::Owned {
                    owner: owner.trim(),
                    name: "",
                };
            }
        }
        match query.split_once('/') {
            Some((owner, name)) if !owner.trim().is_empty() => Self::Owned {
                owner: owner.trim(),
                name: name.trim(),
            },
            _ => Self::Text(query),
        }
    }
}

native_operation!(crate::backend::db::postgres::repositories::discovery::search_repositories,
    crate::backend::db::sqlite::repositories::discovery::search_repositories,
    fn search_repositories(
    pool: &DbPool,
    query: &str,
    limit: i64,
) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::discovery::search_issues,
    crate::backend::db::sqlite::repositories::discovery::search_issues,
    fn search_issues(pool: &DbPool, query: &str, limit: i64) -> Result<Vec<Issue>>);

native_operation!(crate::backend::db::postgres::repositories::discovery::search_users,
    crate::backend::db::sqlite::repositories::discovery::search_users,
    fn search_users(pool: &DbPool, query: &str, limit: i64) -> Result<Vec<User>>);
