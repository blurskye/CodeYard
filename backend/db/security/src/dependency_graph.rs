use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use codeyard_model::dependency_graph::GraphError;
use codeyard_model::dependency_graph::Snapshot;

/// A generated SBOM report can be downloaded for a day, and a repository keeps this many.
pub(crate) const SBOM_REPORT_LIFETIME_SECONDS: i64 = 24 * 60 * 60;
pub(crate) const SBOM_REPORTS_PER_REPOSITORY: i64 = 100;

/// HTTP readers retain the original credential. Scan generations are trusted worker inputs only.
pub enum Reader {
    Viewer(Option<codeyard_db_core::queries::credentials::Actor>),
    Scan(i64),
}

#[derive(sqlx::FromRow)]
pub struct Submission {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    #[sqlx(default)]
    pub updated: bool,
}

native_operation!(crate::postgres::dependency_graph::check_read,
    crate::sqlite::dependency_graph::check_read,
    fn check_read(pool: &DbPool, reader: &Reader, repo: i64) -> Result<(), GraphError>);

native_operation!(crate::postgres::dependency_graph::submit,
    crate::sqlite::dependency_graph::submit,
    fn submit(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, repo: i64, snapshot: &Snapshot)
    -> Result<Submission, GraphError>);

native_operation!(crate::postgres::dependency_graph::snapshots,
    crate::sqlite::dependency_graph::snapshots,
    fn snapshots(pool: &DbPool, reader: &Reader, repo: i64, reference: Option<&str>, sha: Option<&str>)
    -> Result<Vec<Snapshot>, GraphError>);

native_operation!(crate::postgres::dependency_graph::latest_snapshots,
    crate::sqlite::dependency_graph::latest_snapshots,
    fn latest_snapshots(pool: &DbPool, reader: &Reader, repo: i64, reference: &str)
    -> Result<Vec<Snapshot>, GraphError>);

native_operation!(crate::postgres::dependency_graph::save_report,
    crate::sqlite::dependency_graph::save_report,
    fn save_report(pool: &DbPool, reader: &Reader, repo: i64, id: &str, body: &str)
    -> Result<(), GraphError>);

native_operation!(crate::postgres::dependency_graph::prune_expired_reports,
    crate::sqlite::dependency_graph::prune_expired_reports,
    /// Removes expired SBOM reports of every repository; maintenance calls it.
    fn prune_expired_reports(pool: &DbPool) -> Result<u64, GraphError>);

native_operation!(crate::postgres::dependency_graph::report,
    crate::sqlite::dependency_graph::report,
    fn report(pool: &DbPool, reader: &Reader, repo: i64, id: &str)
    -> Result<Option<String>, GraphError>);
