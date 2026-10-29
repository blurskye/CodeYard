//! Issue administration shared by native forms and GraphQL.
use super::*;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Write access is required")]
    Denied,
    #[error("Issue not found")]
    Missing,
    #[error("A repository can pin at most three issues")]
    PinLimit,
    #[error("Choose another repository of the same owner")]
    Destination,
    #[error("An issue cannot duplicate itself")]
    Duplicate,
}

native_operation!(crate::backend::db::postgres::issues::management::closing_candidates,
    crate::backend::db::sqlite::issues::management::closing_candidates,
    fn closing_candidates(pool: &DbPool, viewer: Option<i64>, repo: i64, scope: &str, number: i32) -> Result<Vec<crate::backend::db::models::PullRequest>>);

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct LinkedBranch {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
}

native_operation!(crate::backend::db::postgres::issues::management::duplicate,
    crate::backend::db::sqlite::issues::management::duplicate,
    fn duplicate(pool: &DbPool, issue: i64) -> Result<Option<Issue>>);
native_operation!(crate::backend::db::postgres::issues::management::pinned,
    crate::backend::db::sqlite::issues::management::pinned,
    fn pinned(pool: &DbPool, repo: i64) -> Result<Vec<Issue>>);
native_operation!(crate::backend::db::postgres::issues::management::pin,
    crate::backend::db::sqlite::issues::management::pin,
    fn pin(pool: &DbPool, actor: i64, repo: i64, issue: i64, pinned: bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::issues::management::delete,
    crate::backend::db::sqlite::issues::management::delete,
    fn delete(pool: &DbPool, actor: i64, repo: i64, issue: i64) -> Result<()>);
native_operation!(crate::backend::db::postgres::issues::management::transfer,
    crate::backend::db::sqlite::issues::management::transfer,
    fn transfer(pool: &DbPool, actor: i64, repo: i64, issue: i64, destination: i64) -> Result<Issue>);
native_operation!(crate::backend::db::postgres::issues::management::redirect,
    crate::backend::db::sqlite::issues::management::redirect,
    fn redirect(pool: &DbPool, repo: i64, number: i32) -> Result<Option<Issue>>);
native_operation!(crate::backend::db::postgres::issues::management::close_duplicate,
    crate::backend::db::sqlite::issues::management::close_duplicate,
    fn close_duplicate(pool: &DbPool, actor: i64, repo: i64, issue: i64, duplicate: i64) -> Result<Issue>);
native_operation!(crate::backend::db::postgres::issues::management::linked_branches,
    crate::backend::db::sqlite::issues::management::linked_branches,
    fn linked_branches(pool: &DbPool, issue: i64) -> Result<Vec<LinkedBranch>>);
native_operation!(crate::backend::db::postgres::issues::management::link_branch,
    crate::backend::db::sqlite::issues::management::link_branch,
    fn link_branch(pool: &DbPool, actor: i64, repo: i64, issue: i64, branch_repo: i64, name: &str) -> Result<LinkedBranch>);
