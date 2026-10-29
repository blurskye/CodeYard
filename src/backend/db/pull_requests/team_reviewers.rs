//! Team review requests retain the team identity, never a snapshot of its members.
use crate::backend::db::{models::Team, DbPool};
use anyhow::Result;

#[derive(Debug, thiserror::Error)]
#[error("Review teams must be visible teams with write access to this repository")]
pub struct InvalidReviewTeam;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReviewTeamAction {
    Request,
    Remove,
    Rerequest,
}

native_operation!(crate::backend::db::postgres::pull_requests::team_reviewers::list,
    crate::backend::db::sqlite::pull_requests::team_reviewers::list,
    fn list(pool: &DbPool, repo: i64, pull: Option<i64>, viewer: Option<i64>) -> Result<Vec<crate::backend::db::repositories::push_teams::TeamAccess>>);
native_operation!(crate::backend::db::postgres::pull_requests::team_reviewers::members,
    crate::backend::db::sqlite::pull_requests::team_reviewers::members,
    fn members(pool: &DbPool, repo: i64, team: i64, after: i64) -> Result<Vec<i64>>);
native_operation!(crate::backend::db::postgres::pull_requests::team_reviewers::change,
    crate::backend::db::sqlite::pull_requests::team_reviewers::change,
    fn change(pool: &DbPool, actor: i64, pull: i64, slugs: &[String], action: ReviewTeamAction, expected: Option<&crate::backend::db::models::PullRequest>) -> Result<Vec<Team>>);
