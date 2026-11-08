use codeyard_types::dependency_graph::GraphError;
use codeyard_types::dependency_graph::Snapshot;
pub async fn enabled_in(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {}",
        crate::queries::dependency_graph::enabled_sql(false, "$1")
    )))
    .bind(repo)
    .fetch_one(connection)
    .await
}

/// The caller retains its read snapshot or BEGIN IMMEDIATE reservation through payload loading.
pub async fn bounded_snapshot_ids(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Result<Vec<i64>, GraphError> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(
        crate::queries::dependency_graph::snapshot_candidates_sql(
            false,
            reference.is_some(),
            sha.is_some(),
        ),
    ))
    .bind(repo)
    .bind(reference)
    .bind(sha)
    .fetch_all(connection)
    .await?;
    crate::queries::dependency_graph::snapshot_ids(rows)
}

pub async fn snapshots_in(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Result<Vec<Snapshot>, GraphError> {
    let ids = bounded_snapshot_ids(connection, repo, reference, sha).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<String> = sqlx::query_scalar("SELECT body FROM dependency_snapshots WHERE id IN (SELECT value FROM json_each($1)) ORDER BY body")
        .bind(serde_json::to_string(&ids)?).fetch_all(connection).await?;
    rows.into_iter()
        .map(|body| serde_json::from_str(&body).map_err(GraphError::Stored))
        .collect()
}
