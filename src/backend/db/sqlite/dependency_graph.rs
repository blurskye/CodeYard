use crate::backend::{
    db::dependency_graph::{
        GraphError, Submission, SBOM_REPORTS_PER_REPOSITORY, SBOM_REPORT_LIFETIME_SECONDS,
    },
    dependency_graph::Snapshot,
};
use sqlx::SqlitePool;

/// Only the newest snapshot of each replacement key is read, so older copies are dropped.
/// Submissions and the background dependency scan both call this after saving a snapshot.
pub(super) async fn drop_replaced_snapshots(
    connection: &mut sqlx::SqliteConnection,
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
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    snapshot: &Snapshot,
) -> Result<Submission, GraphError> {
    snapshot.validate()?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !super::discussions::lock_write_authority(&mut tx, actor, repo)
        .await
        .map_err(GraphError::Authority)?
    {
        return Err(GraphError::Denied);
    }
    let mut submission: Submission = sqlx::query_as("INSERT INTO dependency_snapshots(repo_id,sha,reference,correlator,detector,scanned,body) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id,created_at")
        .bind(repo).bind(snapshot.sha.to_ascii_lowercase()).bind(&snapshot.reference)
        .bind(&snapshot.job.correlator).bind(&snapshot.detector.name).bind(snapshot.scanned)
        .bind(serde_json::to_string(snapshot)?).fetch_one(&mut *tx).await?;
    drop_replaced_snapshots(&mut tx, repo, snapshot).await?;
    submission.updated = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM dependency_snapshots WHERE repo_id=$1 AND reference=$2 AND correlator=$3 AND detector=$4 AND scanned>$5)")
        .bind(repo).bind(&snapshot.reference).bind(&snapshot.job.correlator).bind(&snapshot.detector.name).bind(snapshot.scanned).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(submission)
}

pub async fn snapshots(
    pool: &SqlitePool,
    repo: i64,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Result<Vec<Snapshot>, GraphError> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT body FROM (SELECT body,ROW_NUMBER() OVER(PARTITION BY detector,correlator ORDER BY scanned DESC,id DESC) AS position FROM dependency_snapshots WHERE repo_id=$1 AND ($2 IS NULL OR reference=$2) AND ($3 IS NULL OR sha=$3)) ranked WHERE position=1 ORDER BY body")
        .bind(repo).bind(reference).bind(sha).fetch_all(pool).await?;
    rows.into_iter()
        .map(|body| serde_json::from_str(&body).map_err(GraphError::Stored))
        .collect()
}

/// The latest submitted revision is one coherent stored inventory, not per-correlator tips.
pub async fn latest_snapshots(pool: &SqlitePool, repo: i64, reference: &str) -> Result<Vec<Snapshot>, GraphError> {
    let rows: Vec<String> = sqlx::query_scalar("WITH revision AS (SELECT sha FROM dependency_snapshots WHERE repo_id=?1 AND reference=?2 ORDER BY scanned DESC,id DESC LIMIT 1), ranked AS (SELECT body,ROW_NUMBER() OVER(PARTITION BY detector,correlator ORDER BY scanned DESC,id DESC) AS position FROM dependency_snapshots WHERE repo_id=?1 AND reference=?2 AND sha=(SELECT sha FROM revision)) SELECT body FROM ranked WHERE position=1 ORDER BY body")
        .bind(repo).bind(reference).fetch_all(pool).await?;
    rows.into_iter().map(|body| serde_json::from_str(&body).map_err(GraphError::Stored)).collect()
}

pub async fn save_report(
    pool: &SqlitePool,
    repo: i64,
    id: &str,
    body: &str,
) -> Result<(), GraphError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("INSERT INTO dependency_sbom_reports(repo_id,id,body) VALUES($1,$2,$3)")
        .bind(repo)
        .bind(id)
        .bind(body)
        .execute(&mut *tx)
        .await?;
    // Expired reports go, and so does anything beyond the repository's limit, newest kept.
    sqlx::query("DELETE FROM dependency_sbom_reports WHERE repo_id=$1 AND (created_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now','-' || $2 || ' seconds') OR id NOT IN (SELECT id FROM dependency_sbom_reports WHERE repo_id=$1 AND created_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-' || $2 || ' seconds') ORDER BY created_at DESC, id DESC LIMIT $3))")
        .bind(repo)
        .bind(SBOM_REPORT_LIFETIME_SECONDS)
        .bind(SBOM_REPORTS_PER_REPOSITORY)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn prune_expired_reports(pool: &SqlitePool) -> Result<u64, GraphError> {
    Ok(sqlx::query("DELETE FROM dependency_sbom_reports WHERE created_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now','-' || $1 || ' seconds')")
        .bind(SBOM_REPORT_LIFETIME_SECONDS)
        .execute(pool)
        .await?
        .rows_affected())
}

pub async fn report(pool: &SqlitePool, repo: i64, id: &str) -> Result<Option<String>, GraphError> {
    Ok(
        sqlx::query_scalar("SELECT body FROM dependency_sbom_reports WHERE repo_id=$1 AND id=$2 AND created_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-' || $3 || ' seconds')")
            .bind(repo)
            .bind(id)
            .bind(SBOM_REPORT_LIFETIME_SECONDS)
            .fetch_optional(pool)
            .await?,
    )
}
