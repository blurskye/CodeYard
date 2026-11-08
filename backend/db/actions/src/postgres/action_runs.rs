use crate::action_runs::RunMetadata;
use crate::action_runs::RunPage;
use crate::action_runs::RunQuery;
use anyhow::Result;
use codeyard_database::models::CiRun;
use codeyard_database::models::Repository;
use codeyard_database::pool::PgPool;
use sqlx::types::Json;
use sqlx::PgConnection;

/// Called after admission's repository lock, before its transaction commits.
pub async fn record(
    connection: &mut PgConnection,
    run: &CiRun,
    parent: Option<i64>,
) -> Result<CiRun> {
    // Native aggregates are real executions, but not a fictitious comma-named workflow.
    if run.workflow_path.contains(',') {
        return Ok(run.clone());
    }
    let (workflow, number, root, attempt, head, owner) = if let Some(parent) = parent {
        let source:CiRun=sqlx::query_as("SELECT * FROM ci_runs WHERE id=$1 AND repo_id=$2 AND workflow_path=$3 AND head_sha IS NOT DISTINCT FROM $4 AND branch=$5 AND event=$6").bind(parent).bind(run.repo_id).bind(&run.workflow_path).bind(&run.head_sha).bind(&run.branch).bind(&run.event).fetch_optional(&mut *connection).await?.ok_or(codeyard_database::ci_policy::CiAdmissionRefusal::Conflict("Rerun source changed"))?;
        let metadata: RunMetadata =
            sqlx::query_as("SELECT * FROM actions_run_metadata WHERE run_id=$1")
                .bind(parent)
                .fetch_one(&mut *connection)
                .await?;
        let root = source.workflow_run_id.unwrap_or(source.id);
        let attempt: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(workflow_run_attempt),0)+1 FROM ci_runs WHERE workflow_run_id=$1",
        )
        .bind(root)
        .fetch_one(&mut *connection)
        .await?;
        (
            metadata.workflow_id,
            source
                .workflow_run_number
                .ok_or_else(|| anyhow::anyhow!("missing workflow run number"))?,
            root,
            attempt,
            metadata.head_repository,
            metadata.head_owner,
        )
    } else {
        let workflow:i64=sqlx::query_scalar("INSERT INTO actions_workflows(repo_id,path,name) VALUES($1,$2,$2) ON CONFLICT(repo_id,path) DO UPDATE SET path=EXCLUDED.path RETURNING id").bind(run.repo_id).bind(&run.workflow_path).fetch_one(&mut *connection).await?;
        let number:i64=sqlx::query_scalar("UPDATE actions_workflows SET next_run_number=next_run_number+1 WHERE id=$1 RETURNING next_run_number").bind(workflow).fetch_one(&mut *connection).await?;
        let head:Option<Repository>=sqlx::query_as("SELECT * FROM repositories WHERE id=CASE WHEN $2 THEN (SELECT head_repo_id FROM pull_requests WHERE id=$3 AND repo_id=$1) ELSE $1 END").bind(run.repo_id).bind(run.is_fork).bind(run.pull_request_id).fetch_optional(&mut *connection).await?;
        let owner: Option<String> = if let Some(head) = &head {
            sqlx::query_scalar("SELECT COALESCE(u.username,o.name) FROM repositories r LEFT JOIN users u ON u.id=r.owner_user_id AND r.owner_kind='user' LEFT JOIN organizations o ON o.id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=$1").bind(head.id).fetch_optional(&mut *connection).await?
        } else {
            None
        };
        (workflow, number, run.id, 1, head.map(Json), owner)
    };
    sqlx::query("INSERT INTO actions_run_metadata(run_id,workflow_id,head_repository,head_owner) VALUES($1,$2,$3,$4)").bind(run.id).bind(workflow).bind(head).bind(owner).execute(&mut *connection).await?;
    Ok(sqlx::query_as("UPDATE ci_runs SET workflow_run_number=$2,workflow_run_id=$3,workflow_run_attempt=$4 WHERE id=$1 RETURNING *").bind(run.id).bind(number).bind(root).bind(attempt).fetch_one(connection).await?)
}

pub async fn get(pool: &PgPool, repo: i64, id: i64, attempt: Option<i32>) -> Result<Option<CiRun>> {
    let waiting = crate::deployment_protection::waiting_sql(true, "c");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT c.*,{waiting} AS deployment_waiting FROM ci_runs c WHERE c.repo_id=$1 AND COALESCE(c.workflow_run_id,c.id)=$2 AND ($3::integer IS NULL OR COALESCE(c.workflow_run_attempt,1)=$3) ORDER BY COALESCE(c.workflow_run_attempt,1) DESC LIMIT 1"))).bind(repo).bind(id).bind(attempt).fetch_optional(pool).await?)
}

pub async fn metadata(pool: &PgPool, repo: i64, ids: &[i64]) -> Result<Vec<RunMetadata>> {
    Ok(sqlx::query_as("SELECT m.*,origin.created_at AS origin_created_at,origin.triggered_by_id AS actor_id,(SELECT s.id FROM check_suites s WHERE s.ci_run_id=c.id) AS check_suite_id FROM actions_run_metadata m JOIN ci_runs c ON c.id=m.run_id JOIN ci_runs origin ON origin.id=COALESCE(c.workflow_run_id,c.id) WHERE c.repo_id=$1 AND c.id=ANY($2)").bind(repo).bind(ids).fetch_all(pool).await?)
}

const FILTER:&str="c.repo_id=$1 AND c.workflow_path NOT LIKE '%,%' AND NOT EXISTS(SELECT 1 FROM ci_runs newer WHERE newer.workflow_run_id=COALESCE(c.workflow_run_id,c.id) AND newer.workflow_run_attempt>COALESCE(c.workflow_run_attempt,1)) AND ($2='' OR c.workflow_path=$2) AND ($3='' OR EXISTS(SELECT 1 FROM users a WHERE a.id=origin.triggered_by_id AND a.username=$3)) AND ($4='' OR COALESCE(c.pull_request_event->>'head_ref',c.branch) IN ($4,'refs/heads/'||$4,'refs/tags/'||$4)) AND ($5='' OR c.event=$5) AND (NOT $7 OR c.status NOT IN ('queued','pending','running','in_progress')) AND ($8::timestamptz IS NULL OR origin.created_at>$8 OR (NOT $12 AND origin.created_at=$8)) AND ($9::timestamptz IS NULL OR origin.created_at<$9 OR ($11 AND origin.created_at=$9)) AND ($10='' OR c.head_sha=$10) AND ($13::bigint IS NULL OR EXISTS(SELECT 1 FROM check_suites s WHERE s.id=$13 AND s.ci_run_id=c.id))";

pub async fn list(
    pool: &PgPool,
    actor: Option<i64>,
    repo: i64,
    q: &RunQuery,
    offset: i64,
    limit: i64,
) -> Result<Option<RunPage>> {
    anyhow::ensure!(
        offset >= 0 && (1..=100).contains(&limit),
        "invalid workflow run page"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let access = codeyard_database::queries::postgres::repositories::access::USER_REPO_ACCESS_SQL;
    let waiting = crate::deployment_protection::waiting_sql(true, "c");
    let phase = crate::action_runs::state_sql(true, "c");
    let filter = format!("{FILTER} AND (cardinality($6::text[])=0 OR ({phase})=ANY($6))");
    let readable:bool=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$2 WHERE r.id=$1 AND (NOT r.is_private OR (NOT u.is_banned AND NOT u.must_change_password AND ((r.owner_kind='user' AND u.is_admin) OR ({access})>=1))))"))).bind(repo).bind(actor).fetch_one(&mut *tx).await?;
    if !readable {
        return Ok(None);
    }
    let total=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM ci_runs c JOIN ci_runs origin ON origin.id=COALESCE(c.workflow_run_id,c.id) AND origin.repo_id=c.repo_id WHERE {filter}"))).bind(repo).bind(&q.workflow).bind(&q.actor).bind(&q.branch).bind(&q.event).bind(&q.statuses).bind(q.completed).bind(q.since).bind(q.until).bind(&q.sha).bind(q.until_inclusive).bind(q.since_exclusive).bind(q.check_suite).fetch_one(&mut *tx).await?;
    let runs=sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT c.*,{waiting} AS deployment_waiting FROM ci_runs c JOIN ci_runs origin ON origin.id=COALESCE(c.workflow_run_id,c.id) AND origin.repo_id=c.repo_id WHERE {filter} ORDER BY origin.id DESC LIMIT $14 OFFSET $15"))).bind(repo).bind(&q.workflow).bind(&q.actor).bind(&q.branch).bind(&q.event).bind(&q.statuses).bind(q.completed).bind(q.since).bind(q.until).bind(&q.sha).bind(q.until_inclusive).bind(q.since_exclusive).bind(q.check_suite).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(RunPage { total, runs }))
}
