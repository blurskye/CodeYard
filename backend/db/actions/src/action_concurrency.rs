use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::CiRun;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

/// Workflow and job requests share one repository/group ordering and one holder set.
pub(crate) fn blockers_sql(pg: bool) -> String {
    let sql = "SELECT EXISTS(SELECT 1 FROM actions_run_concurrency g JOIN ci_runs c ON c.id=g.run_id WHERE g.repo_id=$1 AND g.name_key=$2 AND (CAST($4 AS BIGINT) IS NULL OR c.id<>$4) AND c.status IN('queued','running') AND (g.holding OR (NOT c.cancel_requested AND g.request_order<$3))) OR EXISTS(SELECT 1 FROM actions_job_concurrency g JOIN actions_jobs j ON j.id=g.job_id JOIN ci_runs c ON c.id=j.run_id WHERE g.repo_id=$1 AND g.name_key=$2 AND (CAST($5 AS BIGINT) IS NULL OR j.id<>$5) AND j.status<>'completed' AND c.status IN('queued','running') AND (g.holding OR (NOT g.cancelled AND NOT c.cancel_requested AND g.request_order<$3)))";
    if pg {
        sql.to_owned()
    } else {
        sql.replace('$', "?")
    }
}

pub(crate) fn groups_sql(pg: bool) -> String {
    let collation = if pg { "\"C\"" } else { "BINARY" };
    let sql = format!("SELECT g.name,g.last_acquired_at FROM actions_concurrency_groups g WHERE g.repo_id=$1 AND (EXISTS(SELECT 1 FROM actions_job_concurrency l JOIN actions_jobs j ON j.id=l.job_id JOIN ci_runs c ON c.id=j.run_id WHERE l.repo_id=g.repo_id AND l.name_key=g.name_key AND ((CAST($2 AS BIGINT) IS NULL AND c.status IN('queued','running') AND j.status<>'completed' AND (l.holding OR NOT l.cancelled)) OR COALESCE(c.workflow_run_id,c.id)=$2)) OR EXISTS(SELECT 1 FROM actions_run_concurrency r JOIN ci_runs c ON c.id=r.run_id WHERE r.repo_id=g.repo_id AND r.name_key=g.name_key AND ((CAST($2 AS BIGINT) IS NULL AND c.status IN('queued','running')) OR COALESCE(c.workflow_run_id,c.id)=$2))) ORDER BY g.name_key COLLATE {collation}");
    if pg {
        sql
    } else {
        sql.replace('$', "?")
    }
}
/// Run-level members, then each unfinished job that holds or waits for the group.
pub(crate) fn members_sql(pg: bool) -> String {
    let sql = "SELECT * FROM (SELECT c.*,g.holding,CAST(NULL AS BIGINT) AS job_id,CAST(NULL AS TEXT) AS job_name FROM ci_runs c JOIN actions_run_concurrency g ON g.run_id=c.id WHERE g.repo_id=$1 AND g.name_key=$2 AND c.status IN('queued','running') UNION ALL SELECT c.*,g.holding,j.id AS job_id,j.name AS job_name FROM ci_runs c JOIN actions_jobs j ON j.run_id=c.id JOIN actions_job_concurrency g ON g.job_id=j.id WHERE g.repo_id=$1 AND g.name_key=$2 AND c.status IN('queued','running') AND j.status<>'completed' AND (g.holding OR NOT g.cancelled)) m ORDER BY holding DESC,id,COALESCE(job_id,0)";
    if pg {
        sql.to_owned()
    } else {
        sql.replace('$', "?")
    }
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
    pub job_id: Option<i64>,
    pub job_name: Option<String>,
}

native_operation!(crate::postgres::action_concurrency::groups,crate::sqlite::action_concurrency::groups,
    fn groups(pool:&DbPool,repo:i64,run:Option<i64>)->Result<Vec<Group>>);
native_operation!(crate::postgres::action_concurrency::members,crate::sqlite::action_concurrency::members,
    fn members(pool:&DbPool,repo:i64,group:&str)->Result<Vec<Member>>);
