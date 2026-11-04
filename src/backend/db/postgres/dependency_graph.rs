use crate::backend::db::dependency_graph::Reader;
use crate::backend::db::dependency_graph::Submission;
use crate::backend::db::dependency_graph::SBOM_REPORTS_PER_REPOSITORY;
use crate::backend::db::dependency_graph::SBOM_REPORT_LIFETIME_SECONDS;
use codeyard_model::dependency_graph::GraphError;
use codeyard_model::dependency_graph::Snapshot;
use sqlx::PgPool;

pub(super) async fn enabled_in(
    connection: &mut sqlx::PgConnection,
    repo: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT {}",
        crate::backend::db::dependency_graph::enabled_sql(true, "$1")
    ))
    .bind(repo)
    .fetch_one(connection)
    .await
}

async fn authorize_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    reader: &Reader,
    repo: i64,
    exclusive: bool,
) -> Result<(), GraphError> {
    match reader {
        Reader::Viewer(Some(actor)) => {
            use crate::backend::db::postgres::repositories::access::lock_readable_repository_in_transaction;
            use crate::backend::db::postgres::repositories::access::RepositoryRowLock;
            let lock = if exclusive {
                RepositoryRowLock::Exclusive
            } else {
                RepositoryRowLock::Shared
            };
            if !lock_readable_repository_in_transaction(tx, actor.user_id, repo, lock)
                .await
                .map_err(GraphError::Authority)?
                || !crate::backend::db::postgres::credentials::repository(
                    tx, actor, repo, "contents", false,
                )
                .await
                .map_err(GraphError::Authority)?
            {
                return Err(GraphError::Denied);
            }
        }
        Reader::Viewer(None) | Reader::Scan(_) => {
            let source_lock = if exclusive {
                "FOR NO KEY UPDATE"
            } else {
                "FOR SHARE"
            };
            crate::backend::db::postgres::repositories::records::lock_fork_ancestors(
                tx,
                repo,
                source_lock,
            )
            .await
            .map_err(GraphError::Authority)?;
            let lock = if exclusive { "FOR UPDATE" } else { "FOR SHARE" };
            let row: Option<i64> = sqlx::query_scalar(&format!("SELECT r.id FROM repositories r LEFT JOIN users u ON FALSE WHERE r.id=$1 AND NOT r.is_gist AND ($2 OR {}) AND (NOT $2 OR NOT r.is_archived) {lock} OF r", crate::backend::db::postgres::repositories::access::readable_condition()))
                .bind(repo).bind(matches!(reader, Reader::Scan(_))).fetch_optional(&mut **tx).await?;
            if row.is_none() {
                return Err(GraphError::Denied);
            }
        }
    }
    if let Reader::Scan(generation) = reader {
        let job: Option<i64> = sqlx::query_scalar(
            "SELECT repo_id FROM vulnerability_jobs WHERE repo_id=$1 AND generation=$2 FOR SHARE",
        )
        .bind(repo)
        .bind(generation)
        .fetch_optional(&mut **tx)
        .await?;
        if job.is_none() {
            return Err(GraphError::Denied);
        }
    }
    if !enabled_in(tx, repo).await? {
        return Err(GraphError::Disabled);
    }
    Ok(())
}

pub async fn check_read(pool: &PgPool, reader: &Reader, repo: i64) -> Result<(), GraphError> {
    let mut tx = pool.begin().await?;
    authorize_in(&mut tx, reader, repo, false).await?;
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
    if !crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
        &mut tx, actor, repo,
    )
    .await
    .map_err(GraphError::Authority)?
    {
        return Err(GraphError::Denied);
    }
    if !crate::backend::db::postgres::credentials::repository(
        &mut tx,
        &credential,
        repo,
        "contents",
        true,
    )
    .await
    .map_err(GraphError::Authority)?
    {
        return Err(GraphError::Denied);
    }
    if !enabled_in(&mut tx, repo).await? {
        return Err(GraphError::Disabled);
    }
    let mut submission: Submission = sqlx::query_as("INSERT INTO dependency_snapshots(repo_id,sha,reference,correlator,detector,scanned,body) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id,created_at")
        .bind(repo).bind(snapshot.sha.to_ascii_lowercase()).bind(&snapshot.reference)
        .bind(&snapshot.job.correlator).bind(&snapshot.detector.name).bind(snapshot.scanned)
        .bind(serde_json::to_string(snapshot)?).fetch_one(&mut *tx).await?;
    drop_replaced_snapshots(&mut tx, repo, snapshot).await?;
    let _ = bounded_snapshot_ids(
        &mut tx,
        repo,
        Some(&snapshot.reference),
        Some(&snapshot.sha.to_ascii_lowercase()),
    )
    .await?;
    submission.updated = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM dependency_snapshots WHERE repo_id=$1 AND reference=$2 AND correlator=$3 AND detector=$4 AND scanned>$5)")
        .bind(repo).bind(&snapshot.reference).bind(&snapshot.job.correlator).bind(&snapshot.detector.name).bind(snapshot.scanned).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(submission)
}

/// The caller retains its read snapshot or repository scope lock through payload loading.
pub(super) async fn bounded_snapshot_ids(
    connection: &mut sqlx::PgConnection,
    repo: i64,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Result<Vec<i64>, GraphError> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        &crate::backend::db::dependency_graph::snapshot_candidates_sql(
            true,
            reference.is_some(),
            sha.is_some(),
        ),
    )
    .bind(repo)
    .bind(reference)
    .bind(sha)
    .fetch_all(connection)
    .await?;
    crate::backend::db::dependency_graph::snapshot_ids(rows)
}

pub(super) async fn snapshots_in(
    connection: &mut sqlx::PgConnection,
    repo: i64,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Result<Vec<Snapshot>, GraphError> {
    let ids = bounded_snapshot_ids(connection, repo, reference, sha).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT body FROM dependency_snapshots WHERE id=ANY($1) ORDER BY body")
            .bind(&ids)
            .fetch_all(connection)
            .await?;
    rows.into_iter()
        .map(|body| serde_json::from_str(&body).map_err(GraphError::Stored))
        .collect()
}

pub async fn snapshots(
    pool: &PgPool,
    reader: &Reader,
    repo: i64,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Result<Vec<Snapshot>, GraphError> {
    let mut tx = pool.begin().await?;
    authorize_in(&mut tx, reader, repo, false).await?;
    let rows = snapshots_in(&mut tx, repo, reference, sha).await?;
    tx.commit().await?;
    Ok(rows)
}

/// The latest submitted revision is one coherent stored inventory, not per-correlator tips.
pub async fn latest_snapshots(
    pool: &PgPool,
    reader: &Reader,
    repo: i64,
    reference: &str,
) -> Result<Vec<Snapshot>, GraphError> {
    let mut tx = pool.begin().await?;
    authorize_in(&mut tx, reader, repo, false).await?;
    let sha: Option<String> = sqlx::query_scalar("SELECT sha FROM dependency_snapshots WHERE repo_id=$1 AND reference=$2 ORDER BY id DESC LIMIT 1")
        .bind(repo).bind(reference).fetch_optional(&mut *tx).await?;
    let rows = match sha {
        Some(sha) => snapshots_in(&mut tx, repo, Some(reference), Some(&sha)).await?,
        None => Vec::new(),
    };
    tx.commit().await?;
    Ok(rows)
}

pub async fn save_report(
    pool: &PgPool,
    reader: &Reader,
    repo: i64,
    id: &str,
    body: &str,
) -> Result<(), GraphError> {
    let mut tx = pool.begin().await?;
    // Read-authorized saves serialize before taking any repository locks, avoiding upgrades.
    authorize_in(&mut tx, reader, repo, true).await?;
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

pub async fn report(
    pool: &PgPool,
    reader: &Reader,
    repo: i64,
    id: &str,
) -> Result<Option<String>, GraphError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(SBOM_REPORT_LIFETIME_SECONDS);
    let mut tx = pool.begin().await?;
    authorize_in(&mut tx, reader, repo, false).await?;
    let body = sqlx::query_scalar(
        "SELECT body FROM dependency_sbom_reports WHERE repo_id=$1 AND id=$2 AND created_at > $3",
    )
    .bind(repo)
    .bind(id)
    .bind(cutoff)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(body)
}
