use crate::queries::runners::Refusal;
use crate::queries::runners::Runner;
use crate::queries::runners::Scope;
use anyhow::Result;
use sqlx::PgConnection as Connection;
pub const RUNNERS:&str="SELECT a.*,false AS online,n.status AS node_status,n.last_seen AS node_last_seen,clock_timestamp() AS observed_at,EXISTS(SELECT 1 FROM ci_runs c WHERE c.status='running' AND c.worker LIKE n.public_key || '/%') AS busy,n.version FROM action_runners a LEFT JOIN nodes n ON n.id=a.node_id";

pub const RUNNER_FILTER: &str = "a.org_id IS NOT DISTINCT FROM $1 AND a.repo_id IS NOT DISTINCT FROM $2 AND ($3::bigint IS NULL OR a.id=$3) AND ($4::bigint IS NULL OR a.group_id=$4) AND ($5::text IS NULL OR LOWER(a.name)=LOWER($5))";

pub async fn runner(c: &mut Connection, scope: Scope, id: i64) -> Result<Runner> {
    let (org, repo) = scope.ids();
    sqlx::query_as::<_, crate::queries::runners::RunnerProjection>(sqlx::AssertSqlSafe(format!(
        "{RUNNERS} WHERE {RUNNER_FILTER} FOR UPDATE OF a"
    )))
    .bind(org)
    .bind(repo)
    .bind(id)
    .bind(None::<i64>)
    .bind(None::<&str>)
    .fetch_optional(c)
    .await?
    .map(|row| row.into_runner())
    .ok_or_else(|| Refusal::Missing.into())
}

/// Removes a repository's own runners and their unused registrations. A transfer calls this:
/// the runners were registered under the former owner's authority.
pub async fn remove_repository_runners(c: &mut Connection, repo: i64) -> Result<()> {
    sqlx::query("UPDATE nodes SET status='revoked' WHERE runner_managed AND id IN (SELECT node_id FROM action_runners WHERE repo_id=$1)")
        .bind(repo)
        .execute(&mut *c)
        .await?;
    sqlx::query("DELETE FROM action_runners WHERE repo_id=$1")
        .bind(repo)
        .execute(&mut *c)
        .await?;
    sqlx::query("DELETE FROM join_tokens WHERE token_hash IN (SELECT token_hash FROM action_runner_tokens WHERE repo_id=$1)")
        .bind(repo)
        .execute(&mut *c)
        .await?;
    sqlx::query("DELETE FROM action_runner_remove_tokens WHERE repo_id=$1")
        .bind(repo)
        .execute(c)
        .await?;
    Ok(())
}

pub async fn retire(c: &mut Connection, worker: Option<&str>) -> Result<()> {
    if let Some(key) = worker.and_then(|w| w.split_once('/').map(|(key, _)| key)) {
        sqlx::query("UPDATE nodes SET status='revoked' WHERE public_key=$1 AND runner_managed AND EXISTS(SELECT 1 FROM action_runners a WHERE a.node_id=nodes.id AND a.ephemeral AND a.used)").bind(key).execute(&mut *c).await?;
        // A used ephemeral runner is gone, as on GitHub, so its name can be registered again.
        sqlx::query("DELETE FROM action_runners WHERE ephemeral AND used AND node_id=(SELECT id FROM nodes WHERE public_key=$1 AND runner_managed)").bind(key).execute(c).await?;
    }
    Ok(())
}
