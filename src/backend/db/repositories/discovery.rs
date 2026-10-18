//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{Issue, PullRequest, Repository, User};
use crate::backend::db::repositories::records::RepositorySort;
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

/// The `ORDER BY` terms of a repository search. Each list ends with the id, so a page never
/// repeats or skips a repository.
pub fn repository_order_by(sort: RepositorySort) -> &'static str {
    match sort {
        RepositorySort::Stars => "r.stars_count DESC, r.updated_at DESC, r.id DESC",
        RepositorySort::Forks => "r.forks_count DESC, r.updated_at DESC, r.id DESC",
        RepositorySort::Updated => "r.updated_at DESC, r.id DESC",
        RepositorySort::Recent => "r.created_at DESC, r.id DESC",
    }
}

/// The order of an issue or pull request search. `Updated` is the best-match default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemSort {
    #[default]
    Updated,
    Newest,
    Oldest,
}

impl ItemSort {
    /// The `ORDER BY` terms for rows of the table aliased `alias`.
    pub fn order_by(self, alias: &str) -> String {
        match self {
            Self::Updated => format!("{alias}.updated_at DESC, {alias}.id DESC"),
            Self::Newest => format!("{alias}.created_at DESC, {alias}.id DESC"),
            Self::Oldest => format!("{alias}.created_at ASC, {alias}.id ASC"),
        }
    }
}

// Each count stops at `cap`, so a count equal to `cap` means at least that many matches.
// Searches are scoped to what `viewer` can read. `None` is an anonymous search, which sees public
// repositories only. Each search and its count apply the same condition, so a page and its total
// agree, and the sort and paging happen in the database over every match.
native_operation!(crate::backend::db::postgres::repositories::discovery::search_repositories,
    crate::backend::db::sqlite::repositories::discovery::search_repositories,
    fn search_repositories(
    pool: &DbPool,
    viewer: Option<i64>,
    query: &str,
    sort: RepositorySort,
    offset: i64,
    limit: i64,
) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::discovery::count_repositories,
    crate::backend::db::sqlite::repositories::discovery::count_repositories,
    fn count_repositories(pool: &DbPool, viewer: Option<i64>, query: &str, cap: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::repositories::discovery::search_issues,
    crate::backend::db::sqlite::repositories::discovery::search_issues,
    fn search_issues(
    pool: &DbPool,
    viewer: Option<i64>,
    query: &str,
    sort: ItemSort,
    offset: i64,
    limit: i64,
) -> Result<Vec<Issue>>);

native_operation!(crate::backend::db::postgres::repositories::discovery::count_issues,
    crate::backend::db::sqlite::repositories::discovery::count_issues,
    fn count_issues(pool: &DbPool, viewer: Option<i64>, query: &str, cap: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::repositories::discovery::search_pull_requests,
    crate::backend::db::sqlite::repositories::discovery::search_pull_requests,
    fn search_pull_requests(
    pool: &DbPool,
    viewer: Option<i64>,
    query: &str,
    sort: ItemSort,
    offset: i64,
    limit: i64,
) -> Result<Vec<PullRequest>>);

native_operation!(crate::backend::db::postgres::repositories::discovery::count_pull_requests,
    crate::backend::db::sqlite::repositories::discovery::count_pull_requests,
    fn count_pull_requests(pool: &DbPool, viewer: Option<i64>, query: &str, cap: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::repositories::discovery::search_users,
    crate::backend::db::sqlite::repositories::discovery::search_users,
    fn search_users(pool: &DbPool, query: &str, offset: i64, limit: i64) -> Result<Vec<User>>);

native_operation!(crate::backend::db::postgres::repositories::discovery::count_users,
    crate::backend::db::sqlite::repositories::discovery::count_users,
    fn count_users(pool: &DbPool, query: &str, cap: i64) -> Result<i64>);
