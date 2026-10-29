//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{Milestone, MilestoneSummary};
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug, sqlx::FromRow)]
pub struct MilestoneDocumentFacts {
    pub creator_id: Option<i64>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

native_operation!(crate::backend::db::postgres::milestones::milestone_document_facts,
    crate::backend::db::sqlite::milestones::milestone_document_facts,
    fn milestone_document_facts(pool: &DbPool, repo: i64, id: i64) -> Result<MilestoneDocumentFacts>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MilestoneWriteRefusal {
    #[error("Milestone mutation denied")]
    Denied,
    #[error("Milestone not found")]
    NotFound,
    #[error("A milestone with this title already exists")]
    DuplicateTitle,
}

pub struct MilestoneInput<'a> {
    pub title: Option<&'a str>,
    pub description: Option<Option<&'a str>>,
    pub due_on: Option<Option<chrono::DateTime<chrono::Utc>>>,
    pub state: Option<&'a str>,
}

pub(crate) fn due_on(day: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    day.map(|day| {
        Ok(chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")?
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc())
    })
    .transpose()
}

native_operation!(crate::backend::db::postgres::milestones::save_milestone,
    crate::backend::db::sqlite::milestones::save_milestone,
    fn save_milestone(pool: &DbPool, actor_id: impl Into<crate::backend::db::users::tokens::Actor>, repo_id: i64, number: Option<i32>, input: MilestoneInput<'_>) -> Result<Milestone>);

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
    fn delete_milestone(pool: &DbPool, actor_id: impl Into<crate::backend::db::users::tokens::Actor>, repo_id: i64, number: i32, expected_id: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::milestones::read_milestones,
    crate::backend::db::sqlite::milestones::read_milestones,
    /// Read authority and milestone payloads share a single statement snapshot.
    fn read_milestones(pool: &DbPool, viewer: Option<i64>, repo: i64, number: Option<i32>, state: Option<&str>, sort: &str, limit: i64, offset: i64) -> Result<Vec<MilestoneSummary>>);

native_operation!(crate::backend::db::postgres::milestones::get_milestone,
    crate::backend::db::sqlite::milestones::get_milestone,
    fn get_milestone(pool: &DbPool, repo_id: i64, number: i32) -> Result<Option<MilestoneSummary>>);

native_operation!(crate::backend::db::postgres::milestones::get_milestone_by_id,
    crate::backend::db::sqlite::milestones::get_milestone_by_id,
    fn get_milestone_by_id(pool: &DbPool, repo_id: i64, id: i64) -> Result<Option<MilestoneSummary>>);

native_operation!(crate::backend::db::postgres::milestones::get_milestones_by_ids,
    crate::backend::db::sqlite::milestones::get_milestones_by_ids,
    /// The milestones of the repository among `ids` (at most 500) with their counts, in no order.
    fn get_milestones_by_ids(pool: &DbPool, repo_id: i64, ids: &[i64]) -> Result<Vec<MilestoneSummary>>);

// Every milestone of a repository, or only those in `state`, with due dates first.
native_operation!(crate::backend::db::postgres::milestones::list_repo_milestones,
    crate::backend::db::sqlite::milestones::list_repo_milestones,
    fn list_repo_milestones(pool: &DbPool, repo_id: i64, state: Option<&str>) -> Result<Vec<MilestoneSummary>>);

/// Picker catalogues are bounded and never load descriptions or progress counts.
pub const MAX_MILESTONE_CHOICES: i64 = 1000;
pub const MILESTONES_PER_PAGE: i64 = 30;

pub(crate) fn milestone_order(sort: &str) -> &'static str {
    match sort {
        "created-asc" => "created_at,number",
        "created-desc" => "created_at DESC,number DESC",
        "updated-asc" => "updated_at,number",
        "updated-desc" => "updated_at DESC,number DESC",
        "number-asc" => "number",
        "number-desc" => "number DESC",
        "due-on-asc" => "due_on IS NULL, due_on, number",
        "due-on-desc" => "due_on IS NULL, due_on DESC, number",
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

pub(crate) fn milestone_search_where(postgres: bool) -> String {
    let position = if postgres { "STRPOS" } else { "INSTR" };
    format!("m.repo_id=$1 AND (CAST($2 AS TEXT) IS NULL OR m.state=$2) AND {position}(LOWER(m.title),LOWER($3))>0")
}
native_operation!(crate::backend::db::postgres::milestones::search_milestones_page,
    crate::backend::db::sqlite::milestones::search_milestones_page,
    fn search_milestones_page(pool:&DbPool,repo_id:i64,state:Option<&str>,query:&str,sort:&str,limit:i64,offset:i64)->Result<Vec<MilestoneSummary>>);
native_operation!(crate::backend::db::postgres::milestones::count_matching_milestones,
    crate::backend::db::sqlite::milestones::count_matching_milestones,
    fn count_matching_milestones(pool:&DbPool,repo_id:i64,state:Option<&str>,query:&str)->Result<i64>);

native_operation!(crate::backend::db::postgres::milestones::list_milestones_page,
    crate::backend::db::sqlite::milestones::list_milestones_page,
    fn list_milestones_page(pool: &DbPool, repo_id: i64, state: Option<&str>, sort: &str, limit: i64, offset: i64) -> Result<Vec<MilestoneSummary>>);

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

// The milestone titled exactly `title`, for a search qualifier the bounded catalogue misses.
native_operation!(crate::backend::db::postgres::milestones::milestone_choice_by_title,
    crate::backend::db::sqlite::milestones::milestone_choice_by_title,
    fn milestone_choice_by_title(pool: &DbPool, repo_id: i64, title: &str) -> Result<Option<Milestone>>);

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
