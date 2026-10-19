//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{Milestone, MilestoneSummary};
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MilestoneWriteRefusal {
    #[error("Milestone mutation denied")]
    Denied,
    #[error("Milestone not found")]
    NotFound,
    #[error("A milestone with this title already exists")]
    DuplicateTitle,
}

native_operation!(crate::backend::db::postgres::milestones::create_milestone,
    crate::backend::db::sqlite::milestones::create_milestone,
    fn create_milestone(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    title: &str,
    description: Option<&str>,
    due_date: Option<&str>,
) -> Result<Milestone>);

native_operation!(crate::backend::db::postgres::milestones::update_milestone,
    crate::backend::db::sqlite::milestones::update_milestone,
    fn update_milestone(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    number: i32,
    title: &str,
    description: Option<&str>,
    due_date: Option<&str>,
) -> Result<Milestone>);

// Sets the state to `open` or `closed`.
native_operation!(crate::backend::db::postgres::milestones::set_milestone_state,
    crate::backend::db::sqlite::milestones::set_milestone_state,
    fn set_milestone_state(pool: &DbPool, actor_id: i64, repo_id: i64, number: i32, state: &str) -> Result<Milestone>);

// Deletes the milestone and clears it from every issue that named it.
native_operation!(crate::backend::db::postgres::milestones::delete_milestone,
    crate::backend::db::sqlite::milestones::delete_milestone,
    fn delete_milestone(pool: &DbPool, actor_id: i64, repo_id: i64, number: i32, expected_id: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::milestones::get_milestone,
    crate::backend::db::sqlite::milestones::get_milestone,
    fn get_milestone(pool: &DbPool, repo_id: i64, number: i32) -> Result<Option<MilestoneSummary>>);

native_operation!(crate::backend::db::postgres::milestones::get_milestone_by_id,
    crate::backend::db::sqlite::milestones::get_milestone_by_id,
    fn get_milestone_by_id(pool: &DbPool, repo_id: i64, id: i64) -> Result<Option<MilestoneSummary>>);

// Every milestone of a repository, or only those in `state`, with due dates first.
native_operation!(crate::backend::db::postgres::milestones::list_repo_milestones,
    crate::backend::db::sqlite::milestones::list_repo_milestones,
    fn list_repo_milestones(pool: &DbPool, repo_id: i64, state: Option<&str>) -> Result<Vec<MilestoneSummary>>);

// Sets or clears (`None`) the milestone of an issue. The milestone is found by its number in
// the same repository.
native_operation!(crate::backend::db::postgres::milestones::set_issue_milestone,
    crate::backend::db::sqlite::milestones::set_issue_milestone,
    fn set_issue_milestone(pool: &DbPool, actor_id: i64, repo_id: i64, issue_id: i64, milestone_number: Option<i32>) -> Result<()>);

// The same for a pull request.
native_operation!(crate::backend::db::postgres::milestones::set_pull_request_milestone,
    crate::backend::db::sqlite::milestones::set_pull_request_milestone,
    fn set_pull_request_milestone(pool: &DbPool, actor_id: i64, repo_id: i64, pull_request_id: i64, milestone_number: Option<i32>) -> Result<()>);
