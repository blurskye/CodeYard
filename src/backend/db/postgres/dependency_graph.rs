use crate::backend::{
    db::dependency_graph::{
        self, GraphError, Submission, SBOM_REPORTS_PER_REPOSITORY, SBOM_REPORT_LIFETIME_SECONDS,
    },
    dependency_graph::Snapshot,
};
use sqlx::PgPool;

pub(super) async fn enabled_in(connection: &mut sqlx::PgConnection, repo: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT {}", dependency_graph::enabled_sql(true, "$1")))
        .bind(repo).fetch_one(connection).await
}

async fn check_enabled_in(connection: &mut sqlx::PgConnection, repo: i64) -> Result<(), GraphError> {
    super::repositories::access::lock_membership_shared(connection).await.map_err(GraphError::Authority)?;
    super::repositories::records::lock_fork_ancestors(connection, repo, "FOR SHARE").await.map_err(GraphError::Authority)?;
    let row: Option<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE id=$1 AND NOT is_gist FOR SHARE")
        .bind(repo).fetch_optional(&mut *connection).await?;
    if row.is_none() { return Err(GraphError::Denied); }
    if !enabled_in(connection, repo).await? { return Err(GraphError::Disabled); }
    Ok(())
}

pub async fn check_enabled(pool: &PgPool, repo: i64) -> Result<(), GraphError> {
    let mut tx = pool.begin().await?;
    check_enabled_in(&mut tx, repo).await?;
    tx.commit().await?;
    Ok(())
}

/// Only the newest snapshot of each replacement key is read, so older copies are dropped.
/// Submissions and the background dependency scan both call this after saving a snapshot.
pub(super) async fn drop_replaced_snapshots(
    connection: &mut sqlx::PgConnection,
    repo: i64,
    snapshot: &Snapshot,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM dependency_snapshots WHERE repo_id=$1 AND sha=$2 AND reference=$3 AND correlator=$4 AND detector=$5 AND id<>(SELECT id FROM dependency_snapshots WHERE repo_id=$1 AND sha=$2 AND reference=$3 AND correlator=$4 AND detector=$5 ORDER BY scanned DESC, id DESC LIMIT 1)")
        .bind(repo)
        .bind(snapshot.sha.to_ascii_lowercase())
        .bind(&snapshot.reference)
        .bind(&snapshot.job.correlator)
        .bind(&snapshot.detector.name)
        .execute(connection)
        .await?;
    Ok(())
}

pub async fn submit(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo: i64,
    snapshot: &Snapshot,
) -> Result<Submission, GraphError> {
    snapshot.validate()?;
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_exclusive_write_authority(&mut tx, actor, repo)
        .await
        .map_err(GraphError::Authority)?
    {
        return Err(GraphError::Denied);
    }
    if !super::credentials::repository(&mut tx, &credential, repo, "contents", true).await.map_err(GraphError::Authority)? {
        return Err(GraphError::Denied);
    }
    if !enabled_in(&mut tx, repo).await? { return Err(GraphError::Disabled); }
    let mut submission: Submission = sqlx::query_as("INSERT INTO dependency_snapshots(repo_id,sha,reference,correlator,detector,scanned,body) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id,created_at")
        .bind(repo).bind(snapshot.sha.to_ascii_lowercase()).bind(&snapshot.reference)
        .bind(&snapshot.job.correlator).bind(&snapshot.detector.name).bind(snapshot.scanned)
        .bind(serde_json::to_string(snapshot)?).fetch_one(&mut *tx).await?;
    drop_replaced_snapshots(&mut tx, repo, snapshot).await?;
    let _ = bounded_snapshot_ids(&mut tx, repo, Some(&snapshot.reference), Some(&snapshot.sha.to_ascii_lowercase())).await?;
    submission.updated = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM dependency_snapshots WHERE repo_id=$1 AND reference=$2 AND correlator=$3 AND detector=$4 AND scanned>$5)")
        .bind(repo).bind(&snapshot.reference).bind(&snapshot.job.correlator).bind(&snapshot.detector.name).bind(snapshot.scanned).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(submission)
}

/// The caller retains its read snapshot or repository write lock through payload loading.
pub(super) async fn bounded_snapshot_ids(
    connection: &mut sqlx::PgConnection, repo: i64, reference: Option<&str>, sha: Option<&str>,
) -> Result<Vec<i64>, GraphError> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(&dependency_graph::snapshot_candidates_sql(true, reference.is_some(), sha.is_some()))
        .bind(repo).bind(reference).bind(sha).fetch_all(connection).await?;
    dependency_graph::snapshot_ids(rows)
}

pub(super) async fn snapshots_in(
    connection: &mut sqlx::PgConnection, repo: i64, reference: Option<&str>, sha: Option<&str>,
) -> Result<Vec<Snapshot>, GraphError> {
    let ids = bounded_snapshot_ids(connection, repo, reference, sha).await?;
    if ids.is_empty() { return Ok(Vec::new()); }
    let rows: Vec<String> = sqlx::query_scalar("SELECT body FROM dependency_snapshots WHERE id=ANY($1) ORDER BY body")
        .bind(&ids).fetch_all(connection).await?;
    rows.into_iter().map(|body| serde_json::from_str(&body).map_err(GraphError::Stored)).collect()
}

pub async fn snapshots(
    pool: &PgPool,
    repo: i64,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Result<Vec<Snapshot>, GraphError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").execute(&mut *tx).await?;
    check_enabled_in(&mut tx, repo).await?;
    let rows = snapshots_in(&mut tx, repo, reference, sha).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn save_report(pool: &PgPool, repo: i64, id: &str, body: &str) -> Result<(), GraphError> {
    let mut tx = pool.begin().await?;
    // Saves of one repository take turns, so concurrent pruning keeps the limit exact.
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR NO KEY UPDATE")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    if !enabled_in(&mut tx, repo).await? { return Err(GraphError::Disabled); }
    sqlx::query("INSERT INTO dependency_sbom_reports(repo_id,id,body) VALUES($1,$2,$3)")
        .bind(repo)
        .bind(id)
        .bind(body)
        .execute(&mut *tx)
        .await?;
    // Expired reports go, and so does anything beyond the repository's limit, newest kept.
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(SBOM_REPORT_LIFETIME_SECONDS);
    sqlx::query("DELETE FROM dependency_sbom_reports WHERE repo_id=$1 AND (created_at <= $2 OR id NOT IN (SELECT id FROM dependency_sbom_reports WHERE repo_id=$1 AND created_at > $2 ORDER BY created_at DESC, id DESC LIMIT $3))")
        .bind(repo)
        .bind(cutoff)
        .bind(SBOM_REPORTS_PER_REPOSITORY)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn prune_expired_reports(pool: &PgPool) -> Result<u64, GraphError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(SBOM_REPORT_LIFETIME_SECONDS);
    Ok(
        sqlx::query("DELETE FROM dependency_sbom_reports WHERE created_at <= $1")
            .bind(cutoff)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

pub async fn report(pool: &PgPool, repo: i64, id: &str) -> Result<Option<String>, GraphError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(SBOM_REPORT_LIFETIME_SECONDS);
    let mut tx = pool.begin().await?;
    check_enabled_in(&mut tx, repo).await?;
    let body = sqlx::query_scalar("SELECT body FROM dependency_sbom_reports WHERE repo_id=$1 AND id=$2 AND created_at > $3")
        .bind(repo).bind(id).bind(cutoff).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(body)
}
