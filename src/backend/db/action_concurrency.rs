//! Workflow-level leases persist with run configuration after the queue is empty.
use super::{models::CiRun, DbPool};
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug)]
pub struct Configuration {
    pub group: String,
    pub cancel_in_progress: bool,
}
impl Configuration {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.group.is_empty()
                && self.group.len() <= 256
                && !self.group.chars().any(char::is_control),
            super::ci::CiAdmissionRefusal::Invalid("Invalid concurrency group")
        );
        Ok(())
    }
}
/// Workflow and job requests share one repository/group ordering and one holder set.
pub(crate) fn blockers_sql(pg: bool) -> String {
    let sql = "SELECT EXISTS(SELECT 1 FROM actions_run_concurrency g JOIN ci_runs c ON c.id=g.run_id WHERE g.repo_id=$1 AND g.name_key=$2 AND (CAST($4 AS BIGINT) IS NULL OR c.id<>$4) AND c.status IN('queued','running') AND (g.holding OR (NOT c.cancel_requested AND g.request_order<$3))) OR EXISTS(SELECT 1 FROM actions_job_concurrency g JOIN actions_jobs j ON j.id=g.job_id JOIN ci_runs c ON c.id=j.run_id WHERE g.repo_id=$1 AND g.name_key=$2 AND (CAST($5 AS BIGINT) IS NULL OR j.id<>$5) AND j.status<>'completed' AND c.status IN('queued','running') AND (g.holding OR (NOT g.cancelled AND NOT c.cancel_requested AND g.request_order<$3)))";
    if pg { sql.to_owned() } else { sql.replace('$', "?") }
}

pub(crate) fn groups_sql(pg: bool) -> String {
    let collation = if pg { "\"C\"" } else { "BINARY" };
    let sql = format!("SELECT g.name,g.last_acquired_at FROM actions_concurrency_groups g WHERE g.repo_id=$1 AND (EXISTS(SELECT 1 FROM actions_run_concurrency m JOIN ci_runs c ON c.id=m.run_id WHERE m.repo_id=g.repo_id AND m.name_key=g.name_key AND ((CAST($2 AS BIGINT) IS NULL AND c.status IN('queued','running')) OR COALESCE(c.workflow_run_id,c.id)=$2)) OR EXISTS(SELECT 1 FROM actions_job_concurrency m JOIN actions_jobs j ON j.id=m.job_id JOIN ci_runs c ON c.id=j.run_id WHERE m.repo_id=g.repo_id AND m.name_key=g.name_key AND ((CAST($2 AS BIGINT) IS NULL AND c.status IN('queued','running') AND j.status<>'completed') OR COALESCE(c.workflow_run_id,c.id)=$2))) ORDER BY g.name_key COLLATE {collation}");
    if pg { sql } else { sql.replace('$', "?") }
}
pub(crate) fn members_sql(pg: bool) -> String {
    let run = "EXISTS(SELECT 1 FROM actions_run_concurrency g WHERE g.run_id=c.id AND g.repo_id=c.repo_id AND g.name_key=$2";
    let job = "EXISTS(SELECT 1 FROM actions_job_concurrency g JOIN actions_jobs j ON j.id=g.job_id WHERE j.run_id=c.id AND g.repo_id=c.repo_id AND g.name_key=$2 AND j.status<>'completed'";
    let sql = format!("SELECT c.*,({run} AND g.holding) OR {job} AND g.holding)) AS holding FROM ci_runs c WHERE c.repo_id=$1 AND c.status IN('queued','running') AND ({run}) OR {job})) ORDER BY holding DESC,c.id");
    if pg { sql } else { sql.replace('$', "?") }
}

#[derive(sqlx::FromRow)]
pub struct Group {
    pub name: String,
    pub last_acquired_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
pub struct Member {
    #[sqlx(flatten)]
    pub run: CiRun,
    pub holding: bool,
}

native_operation!(crate::backend::db::postgres::action_concurrency::groups,crate::backend::db::sqlite::action_concurrency::groups,
    fn groups(pool:&DbPool,repo:i64,run:Option<i64>)->Result<Vec<Group>>);
native_operation!(crate::backend::db::postgres::action_concurrency::members,crate::backend::db::sqlite::action_concurrency::members,
    fn members(pool:&DbPool,repo:i64,group:&str)->Result<Vec<Member>>);
