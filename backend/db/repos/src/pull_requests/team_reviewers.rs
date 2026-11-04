use anyhow::Result;
use codeyard_db_core::models::Team;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(Debug, thiserror::Error)]
#[error("Review teams must be visible teams with write access to this repository")]
pub struct InvalidReviewTeam;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReviewTeamAction {
    Request,
    Remove,
    Rerequest,
}

pub struct RequestWindow {
    pub pull: i64,
    pub repo: i64,
    pub viewer: Option<i64>,
    pub target: Option<(bool, i64)>,
    pub offset: i64,
    pub limit: i64,
    pub count: bool,
}

pub(crate) fn request_roster_sql(prefix: &str, teams: &str, readable: &str) -> String {
    format!("SELECT user_id,team_id,as_code_owner,kind,sort_name FROM (SELECT q.user_id,CAST(NULL AS BIGINT) AS team_id,q.as_code_owner,0 AS kind,author.username AS sort_name FROM pr_reviewers q JOIN users author ON author.id=q.user_id WHERE q.pull_request_id={prefix}3 UNION ALL SELECT CAST(NULL AS BIGINT),q.team_id,q.as_code_owner,1,t.name FROM pr_team_reviewers q JOIN teams t ON t.id=q.team_id WHERE q.pull_request_id={prefix}3) requests WHERE (team_id IS NULL OR team_id IN (SELECT id FROM ({teams}) visible)) AND EXISTS(SELECT 1 FROM pull_requests p JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id={prefix}2 WHERE p.id={prefix}3 AND p.repo_id={prefix}1 AND {readable}) AND (CAST({prefix}4 AS BOOLEAN) IS NULL OR (team_id IS NOT NULL)={prefix}4) AND (CAST({prefix}5 AS BIGINT) IS NULL OR COALESCE(user_id,team_id)={prefix}5)")
}
native_operation!(crate::postgres::pull_requests::team_reviewers::request_page,
    crate::sqlite::pull_requests::team_reviewers::request_page,
    fn request_page(pool: &DbPool, window: &RequestWindow) -> Result<(i64, Vec<crate::pull_requests::reviews::ReviewRequestOrigin>)>);
native_operation!(crate::postgres::pull_requests::team_reviewers::by_ids,
    crate::sqlite::pull_requests::team_reviewers::by_ids,
    fn by_ids(pool: &DbPool, repo: i64, viewer: Option<i64>, ids: &[i64]) -> Result<Vec<Team>>);

pub struct ReviewRequests<'a> {
    pub users: &'a [i64],
    pub teams: &'a [i64],
    pub union: bool,
    pub rerequest: bool,
    pub token: Option<i64>,
}
pub struct ReviewRequestChanges {
    pub added_users: Vec<User>,
    pub added_teams: Vec<Team>,
}
native_operation!(crate::postgres::pull_requests::team_reviewers::set_requests,
    crate::sqlite::pull_requests::team_reviewers::set_requests,
    fn set_requests(pool: &DbPool, actor: i64, pull: i64, input: &ReviewRequests<'_>) -> Result<ReviewRequestChanges>);

native_operation!(crate::postgres::pull_requests::team_reviewers::list,
    crate::sqlite::pull_requests::team_reviewers::list,
    fn list(pool: &DbPool, repo: i64, pull: Option<i64>, viewer: Option<i64>) -> Result<Vec<crate::repositories::push_teams::TeamAccess>>);
native_operation!(crate::postgres::pull_requests::team_reviewers::members,
    crate::sqlite::pull_requests::team_reviewers::members,
    fn members(pool: &DbPool, repo: i64, teams: &[i64], after: i64) -> Result<Vec<User>>);
native_operation!(crate::postgres::pull_requests::team_reviewers::change,
    crate::sqlite::pull_requests::team_reviewers::change,
    fn change(pool: &DbPool, actor: i64, pull: i64, slugs: &[String], action: ReviewTeamAction, expected: Option<&codeyard_db_core::models::PullRequest>) -> Result<Vec<Team>>);
