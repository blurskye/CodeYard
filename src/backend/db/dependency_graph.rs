//! Snapshot history and completed SBOM reports survive process restarts.
use super::DbPool;
use crate::backend::dependency_graph::Snapshot;
use chrono::{DateTime, Utc};

/// A generated SBOM report can be downloaded for a day, and a repository keeps this many.
pub(crate) const SBOM_REPORT_LIFETIME_SECONDS: i64 = 24 * 60 * 60;
pub(crate) const SBOM_REPORTS_PER_REPOSITORY: i64 = 100;

/// One reported default-branch revision, ordered by server receipt rather than detector clocks.
/// Callers bind the repository as $1; all detectors contributing alerts must use this SHA.
pub(crate) const DEFAULT_SNAPSHOT_SHA_SQL: &str = "(SELECT d.sha FROM dependency_snapshots d JOIN repositories r ON r.id=d.repo_id WHERE d.repo_id=$1 AND d.reference='refs/heads/'||r.default_branch ORDER BY d.id DESC LIMIT 1)";

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("Dependency graph access denied")]
    Denied,
    #[error("Invalid dependency snapshot")]
    Invalid,
    #[error("Dependency graph database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Dependency graph authority unavailable")]
    Authority(#[source] anyhow::Error),
    #[error("Invalid stored dependency graph")]
    Stored(#[from] serde_json::Error),
}

#[derive(sqlx::FromRow)]
pub struct Submission {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    #[sqlx(default)]
    pub updated: bool,
}

native_operation!(super::postgres::dependency_graph::submit,
    super::sqlite::dependency_graph::submit,
    fn submit(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, snapshot: &Snapshot)
    -> Result<Submission, GraphError>);

native_operation!(super::postgres::dependency_graph::snapshots,
    super::sqlite::dependency_graph::snapshots,
    fn snapshots(pool: &DbPool, repo: i64, reference: Option<&str>, sha: Option<&str>)
    -> Result<Vec<Snapshot>, GraphError>);

native_operation!(super::postgres::dependency_graph::save_report,
    super::sqlite::dependency_graph::save_report,
    fn save_report(pool: &DbPool, repo: i64, id: &str, body: &str)
    -> Result<(), GraphError>);

native_operation!(super::postgres::dependency_graph::prune_expired_reports,
    super::sqlite::dependency_graph::prune_expired_reports,
    /// Removes expired SBOM reports of every repository; maintenance calls it.
    fn prune_expired_reports(pool: &DbPool) -> Result<u64, GraphError>);

native_operation!(super::postgres::dependency_graph::report,
    super::sqlite::dependency_graph::report,
    fn report(pool: &DbPool, repo: i64, id: &str)
    -> Result<Option<String>, GraphError>);
