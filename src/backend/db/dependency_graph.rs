//! Snapshot history and completed SBOM reports survive process restarts.
use chrono::{DateTime, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use codeyard_model::dependency_graph::Snapshot;

/// A generated SBOM report can be downloaded for a day, and a repository keeps this many.
pub(crate) const SBOM_REPORT_LIFETIME_SECONDS: i64 = 24 * 60 * 60;
pub(crate) const SBOM_REPORTS_PER_REPOSITORY: i64 = 100;

/// One reported default-branch revision, ordered by server receipt rather than detector clocks.
/// Callers bind the repository as $1; all detectors contributing alerts must use this SHA.
pub(crate) const DEFAULT_SNAPSHOT_SHA_SQL: &str = "(SELECT d.sha FROM dependency_snapshots d JOIN repositories r ON r.id=d.repo_id WHERE d.repo_id=$1 AND d.reference='refs/heads/'||r.default_branch ORDER BY d.id DESC LIMIT 1)";

/// Whether the dependency graph is on, as on GitHub: an applied setting decides; without one it
/// is on for a public repository, and on for a private one only once Dependabot alerts are
/// enabled for it with the owner's consent to read its private dependencies.
pub(crate) fn enabled_sql(pg: bool, repo: &str) -> String {
    let status = if pg {
        "(g.security_and_analysis::jsonb)->'dependency_graph'->>'status'"
    } else {
        "json_extract(g.security_and_analysis,'$.dependency_graph.status')"
    };
    format!("CASE (SELECT {status} FROM repository_security_settings g WHERE g.repo_id={repo}) WHEN 'disabled' THEN FALSE WHEN 'enabled' THEN TRUE ELSE EXISTS(SELECT 1 FROM repositories p WHERE p.id={repo} AND (NOT p.is_private OR EXISTS(SELECT 1 FROM repository_dependency_security d WHERE d.repo_id=p.id AND d.alerts_enabled AND d.private_consent))) END")
}

pub(crate) const MAX_SNAPSHOT_SOURCES: usize = 100;
pub(crate) const MAX_SNAPSHOT_BYTES: i64 = 4 * 1024 * 1024;

pub(crate) fn snapshot_candidates_sql(pg: bool, reference: bool, sha: bool) -> String {
    let reference = if reference {
        "reference=$2"
    } else {
        "CAST($2 AS TEXT) IS NULL"
    };
    let sha = if sha {
        "sha=$3"
    } else {
        "CAST($3 AS TEXT) IS NULL"
    };
    let bytes = if pg {
        "octet_length(s.body)"
    } else {
        "length(CAST(s.body AS BLOB))"
    };
    let limit = MAX_SNAPSHOT_SOURCES + 1;
    format!("WITH ranked AS (SELECT id,ROW_NUMBER() OVER(PARTITION BY detector,correlator ORDER BY scanned DESC,id DESC) AS position FROM dependency_snapshots WHERE repo_id=$1 AND {reference} AND {sha}), candidates AS (SELECT id FROM ranked WHERE position=1 ORDER BY id LIMIT {limit}) SELECT s.id,CAST({bytes} AS BIGINT) FROM candidates c JOIN dependency_snapshots s ON s.id=c.id ORDER BY s.id")
}

pub(crate) fn snapshot_ids(rows: Vec<(i64, i64)>) -> Result<Vec<i64>, GraphError> {
    if rows.len() > MAX_SNAPSHOT_SOURCES
        || rows.iter().map(|(_, bytes)| bytes).sum::<i64>() > MAX_SNAPSHOT_BYTES
    {
        return Err(GraphError::Limit);
    }
    Ok(rows.into_iter().map(|(id, _)| id).collect())
}

/// HTTP readers retain the original credential. Scan generations are trusted worker inputs only.
pub enum Reader {
    Viewer(Option<crate::backend::db::credentials::Actor>),
    Scan(i64),
}

#[derive(sqlx::FromRow)]
pub struct Submission {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    #[sqlx(default)]
    pub updated: bool,
}

native_operation!(crate::backend::db::postgres::dependency_graph::check_read,
    crate::backend::db::sqlite::dependency_graph::check_read,
    fn check_read(pool: &DbPool, reader: &Reader, repo: i64) -> Result<(), GraphError>);

native_operation!(crate::backend::db::postgres::dependency_graph::submit,
    crate::backend::db::sqlite::dependency_graph::submit,
    fn submit(pool: &DbPool, actor: impl Into<crate::backend::db::credentials::Actor>, repo: i64, snapshot: &Snapshot)
    -> Result<Submission, GraphError>);

native_operation!(crate::backend::db::postgres::dependency_graph::snapshots,
    crate::backend::db::sqlite::dependency_graph::snapshots,
    fn snapshots(pool: &DbPool, reader: &Reader, repo: i64, reference: Option<&str>, sha: Option<&str>)
    -> Result<Vec<Snapshot>, GraphError>);

native_operation!(crate::backend::db::postgres::dependency_graph::latest_snapshots,
    crate::backend::db::sqlite::dependency_graph::latest_snapshots,
    fn latest_snapshots(pool: &DbPool, reader: &Reader, repo: i64, reference: &str)
    -> Result<Vec<Snapshot>, GraphError>);

native_operation!(crate::backend::db::postgres::dependency_graph::save_report,
    crate::backend::db::sqlite::dependency_graph::save_report,
    fn save_report(pool: &DbPool, reader: &Reader, repo: i64, id: &str, body: &str)
    -> Result<(), GraphError>);

native_operation!(crate::backend::db::postgres::dependency_graph::prune_expired_reports,
    crate::backend::db::sqlite::dependency_graph::prune_expired_reports,
    /// Removes expired SBOM reports of every repository; maintenance calls it.
    fn prune_expired_reports(pool: &DbPool) -> Result<u64, GraphError>);

native_operation!(crate::backend::db::postgres::dependency_graph::report,
    crate::backend::db::sqlite::dependency_graph::report,
    fn report(pool: &DbPool, reader: &Reader, repo: i64, id: &str)
    -> Result<Option<String>, GraphError>);

use codeyard_model::dependency_graph::GraphError;
