use crate::backups::PullRequestHead;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_db_core::pool::PgPool;

pub async fn export_snapshot(
    pool: &PgPool,
) -> Result<(
    sqlx::Transaction<'static, sqlx::Postgres>,
    String,
    Vec<Repository>,
    Vec<crate::objects::Object>,
    Vec<(i64, String)>,
)> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let snapshot = sqlx::query_scalar("SELECT pg_export_snapshot()")
        .fetch_one(&mut *tx)
        .await?;
    let limit = crate::backups::entry_limit()?;
    let repositories: Vec<Repository> =
        sqlx::query_as("SELECT * FROM repositories ORDER BY id LIMIT $1")
            .bind(limit + 1)
            .fetch_all(&mut *tx)
            .await?;
    anyhow::ensure!(
        repositories.len() as i64 <= limit,
        "backup repository limit exceeded"
    );
    let objects:Vec<crate::objects::Object>=sqlx::query_as("SELECT object_key,digest,bytes,content_type,confirmed FROM storage_objects WHERE confirmed=1 ORDER BY object_key LIMIT $1")
        .bind(limit+1).fetch_all(&mut *tx).await?;
    anyhow::ensure!(
        objects.len() as i64 <= limit,
        "backup object limit exceeded"
    );
    let max_refs = crate::backups::reference_limit()?;
    let revisions:Vec<(i64,String)>=sqlx::query_as("SELECT repo_id,SUBSTR(oid,1,65) FROM (SELECT repo_id,head_sha AS oid FROM pull_requests UNION SELECT repo_id,base_sha FROM pull_requests UNION SELECT repo_id,fork_sha FROM pull_requests UNION SELECT repo_id,merge_commit_sha FROM pull_requests UNION SELECT repo_id,head_sha FROM ci_runs WHERE status IN ('queued','pending','running','in_progress')) revisions WHERE oid IS NOT NULL AND oid<>'' ORDER BY repo_id,oid LIMIT $1")
        .bind(max_refs+1).fetch_all(&mut *tx).await?;
    anyhow::ensure!(
        revisions.len() as i64 <= max_refs
            && revisions
                .iter()
                .all(|(_, id)| codeyard_git::is_full_commit_id(id)),
        "invalid or excessive snapshot revisions"
    );
    Ok((tx, snapshot, repositories, objects, revisions))
}

pub async fn list_repositories(pool: &PgPool) -> Result<Vec<Repository>> {
    let limit = crate::backups::entry_limit()?;
    let rows: Vec<Repository> = sqlx::query_as("SELECT * FROM repositories ORDER BY id LIMIT $1")
        .bind(limit + 1)
        .fetch_all(pool)
        .await?;
    anyhow::ensure!(
        rows.len() as i64 <= limit,
        "backup repository limit exceeded"
    );
    Ok(rows)
}

pub async fn set_disk_path(pool: &PgPool, repo_id: i64, disk_path: &str) -> Result<()> {
    sqlx::query("UPDATE repositories SET disk_path = $2 WHERE id = $1")
        .bind(repo_id)
        .bind(disk_path)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_pull_request_heads(pool: &PgPool, repo_id: i64) -> Result<Vec<PullRequestHead>> {
    Ok(sqlx::query_as(
        "SELECT id, number::BIGINT AS number, state, head_sha, head_repo_id, head_branch, base_sha, base_branch, fork_sha, merge_commit_sha FROM pull_requests WHERE repo_id = $1 ORDER BY number",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_ci_revisions(pool: &PgPool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT head_sha FROM ci_runs WHERE repo_id = $1 AND head_sha IS NOT NULL AND status IN ('queued', 'pending', 'running', 'in_progress') ORDER BY head_sha",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

pub async fn is_empty(pool: &PgPool) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = current_schema())",
    )
    .fetch_one(pool)
    .await?)
}

pub async fn restore_refs(
    pool: &PgPool,
    repository: i64,
    refs: &[codeyard_db_core::models::RefValue],
    force: bool,
) -> Result<()> {
    crate::cluster::writes::validate_values(refs)?;
    let mut tx = pool.begin().await?;
    let pending: bool =
        sqlx::query_scalar("SELECT restore_pending FROM node_configuration WHERE id=1")
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(pending, "cluster restore is not pending");
    let control:Option<(i64,i64,i32)>=sqlx::query_as("SELECT term,committed_version,serving FROM repository_write_control WHERE repository_id=$1 FOR UPDATE")
        .bind(repository).fetch_optional(&mut *tx).await?;
    let Some((term, version, serving)) = control else {
        return Ok(());
    };
    anyhow::ensure!(serving == 0, "restore repository is already serving");
    if !force {
        let current:Vec<codeyard_db_core::models::RefValue>=sqlx::query_as("SELECT name,kind,target FROM repository_committed_refs WHERE repository_id=$1 ORDER BY name LIMIT $2")
            .bind(repository).bind((crate::cluster::writes::MAX_REFS+1) as i64).fetch_all(&mut *tx).await?;
        let normalized = |values: &[codeyard_db_core::models::RefValue]| {
            values
                .iter()
                .map(|value| (value.name.clone(), value.kind.clone(), value.target.clone()))
                .collect::<std::collections::BTreeSet<_>>()
        };
        if normalized(&current) == normalized(refs) {
            return Ok(());
        }
    }
    let term = term
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("restore term overflow"))?;
    let version = version
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("restore version overflow"))?;
    sqlx::query("UPDATE repository_writes SET decision=CASE WHEN decision='pending' THEN 'abort' ELSE decision END,finished=1 WHERE repository_id=$1 AND finished=0").bind(repository).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM repository_committed_refs WHERE repository_id=$1")
        .bind(repository)
        .execute(&mut *tx)
        .await?;
    for value in refs {
        sqlx::query("INSERT INTO repository_committed_refs(repository_id,name,kind,target) VALUES($1,$2,$3,$4)")
            .bind(repository).bind(&value.name).bind(&value.kind).bind(&value.target).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE repository_write_control SET term=$2,committed_version=$3 WHERE repository_id=$1 AND serving=0").bind(repository).bind(term).bind(version).execute(&mut *tx).await?;
    sqlx::query("UPDATE repository_voters SET term=$2,applied_version=-1 WHERE repository_id=$1")
        .bind(repository)
        .bind(term)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE repository_replicas SET applied_version=-1,synced_at=NULL WHERE repository_id=$1",
    )
    .bind(repository)
    .execute(&mut *tx)
    .await?;
    let changed=sqlx::query("UPDATE repositories SET write_version=write_version+1 WHERE id=$1 AND write_version<9223372036854775807")
        .bind(repository).execute(&mut *tx).await?;
    anyhow::ensure!(changed.rows_affected() == 1, "restore version overflow");
    tx.commit().await?;
    Ok(())
}

pub async fn restore_node_ready(pool: &PgPool, node: i64, key: &str) -> Result<()> {
    let changed=sqlx::query("UPDATE nodes SET state='active',drain_error=NULL WHERE id=$1 AND public_key=$2 AND status='active' AND last_seen IS NULL AND EXISTS(SELECT 1 FROM node_configuration WHERE restore_pending)")
        .bind(node).bind(key).execute(pool).await?;
    anyhow::ensure!(
        changed.rows_affected() == 1,
        "restored node identity changed or is already serving"
    );
    Ok(())
}

pub async fn node_objects(pool: &PgPool, node: i64) -> Result<Vec<crate::objects::Object>> {
    let limit = crate::backups::entry_limit()?;
    let objects:Vec<crate::objects::Object>=sqlx::query_as("SELECT o.object_key,o.digest,o.bytes,o.content_type,o.confirmed FROM storage_objects o JOIN storage_object_copies c ON c.object_key=o.object_key WHERE c.node_id=$1 AND o.confirmed=1 ORDER BY o.object_key LIMIT $2")
        .bind(node).bind(limit+1).fetch_all(pool).await?;
    anyhow::ensure!(
        objects.len() as i64 <= limit,
        "restore object limit exceeded"
    );
    Ok(objects)
}

pub async fn restore_pending(pool: &PgPool) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT restore_pending FROM node_configuration WHERE id=1")
            .fetch_optional(pool)
            .await?
            .unwrap_or(false),
    )
}
pub async fn finish_restore(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;
    let pending: bool =
        sqlx::query_scalar("SELECT restore_pending FROM node_configuration WHERE id=1 FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(pending, "cluster restore is not pending");
    let incomplete:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN nodes n ON n.id=r.storage_node_id WHERE n.status<>'active' OR n.state<>'active') OR EXISTS(SELECT 1 FROM repository_write_control c WHERE (SELECT COUNT(*) FROM repository_voters v WHERE v.repository_id=c.repository_id AND v.term=c.term)<2 OR EXISTS(SELECT 1 FROM repository_voters v JOIN nodes n ON n.id=v.node_id WHERE v.repository_id=c.repository_id AND (v.term<>c.term OR v.applied_version<>c.committed_version OR n.status<>'active' OR n.state<>'active'))) OR EXISTS(SELECT 1 FROM storage_objects o WHERE o.confirmed=1 AND (SELECT COUNT(*) FROM storage_object_copies c JOIN nodes n ON n.id=c.node_id WHERE c.object_key=o.object_key AND c.ready=1 AND n.status='active' AND n.state='active')<2)").fetch_one(&mut *tx).await?;
    anyhow::ensure!(
        !incomplete,
        "restore every assigned storage node before completing verification"
    );
    sqlx::query(
        "UPDATE node_configuration SET restore_pending=FALSE,restore_digest=NULL WHERE id=1",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn schema_name(pool: &PgPool) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(current_schema(),'public')")
            .fetch_one(pool)
            .await?,
    )
}

pub async fn restore_digest(pool: &PgPool) -> Result<Option<String>> {
    let value: Option<Option<String>> = sqlx::query_scalar(
        "SELECT SUBSTR(restore_digest,1,65) FROM node_configuration WHERE id=1 AND restore_pending",
    )
    .fetch_optional(pool)
    .await?;
    Ok(value.flatten())
}
