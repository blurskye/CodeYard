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

/// Picker catalogues are bounded and never load descriptions or progress counts.
pub const MAX_MILESTONE_CHOICES: i64 = 1000;
pub const MILESTONES_PER_PAGE: i64 = 30;

pub(crate) fn milestone_order(sort: &str) -> &'static str {
    match sort {
        "furthest-due" => "due_date IS NULL, due_date DESC, number",
        "most-complete" => {
            "COALESCE(100 * closed_items / NULLIF(open_items + closed_items, 0), 0) DESC, number"
        }
        "least-complete" => {
            "COALESCE(100 * closed_items / NULLIF(open_items + closed_items, 0), 0), number"
        }
        "most-issues" => "open_items + closed_items DESC, number",
        "least-issues" => "open_items + closed_items, number",
        "alphabetical" => "lower(title), number",
        "reverse-alphabetical" => "lower(title) DESC, number",
        _ => "due_date IS NULL, due_date, number",
    }
}

native_operation!(crate::backend::db::postgres::milestones::list_milestones_page,
    crate::backend::db::sqlite::milestones::list_milestones_page,
    fn list_milestones_page(pool: &DbPool, repo_id: i64, state: &str, sort: &str, offset: i64) -> Result<Vec<MilestoneSummary>>);

native_operation!(crate::backend::db::postgres::milestones::count_milestones,
    crate::backend::db::sqlite::milestones::count_milestones,
    fn count_milestones(pool: &DbPool, repo_id: i64) -> Result<(i64, i64)>);

native_operation!(crate::backend::db::postgres::milestones::list_milestone_filters,
    crate::backend::db::sqlite::milestones::list_milestone_filters,
    fn list_milestone_filters(pool: &DbPool, repo_id: i64) -> Result<Vec<MilestoneSummary>>);

// Milestones for pickers, without counting linked issues or pull requests.
native_operation!(crate::backend::db::postgres::milestones::list_milestone_choices,
    crate::backend::db::sqlite::milestones::list_milestone_choices,
    fn list_milestone_choices(pool: &DbPool, repo_id: i64) -> Result<Vec<Milestone>>);

native_operation!(crate::backend::db::postgres::milestones::milestone_choices_by_ids,
    crate::backend::db::sqlite::milestones::milestone_choices_by_ids,
    fn milestone_choices_by_ids(pool: &DbPool, repo_id: i64, ids: &[i64]) -> Result<Vec<Milestone>>);

// Sets or clears (`None`) the milestone of an issue. The milestone is found by its number in
// the same repository.
native_operation!(crate::backend::db::postgres::milestones::set_issue_milestone,
    crate::backend::db::sqlite::milestones::set_issue_milestone,
    fn set_issue_milestone(pool: &DbPool, actor_id: i64, repo_id: i64, issue_id: i64, milestone_number: Option<i32>) -> Result<()>);

// The same for a pull request.
native_operation!(crate::backend::db::postgres::milestones::set_pull_request_milestone,
    crate::backend::db::sqlite::milestones::set_pull_request_milestone,
    fn set_pull_request_milestone(pool: &DbPool, actor_id: i64, repo_id: i64, pull_request_id: i64, milestone_number: Option<i32>) -> Result<()>);
