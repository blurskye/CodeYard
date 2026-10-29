//! The rows backups, restores and repository checks read and repair.
use crate::backend::db::models::Repository;
use crate::backend::db::DbPool;
use anyhow::{bail, Result};

/// The recorded revisions of one pull request.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PullRequestHead {
    pub id: i64,
    pub number: i64,
    pub state: String,
    pub head_sha: String,
    pub base_sha: String,
    pub fork_sha: Option<String>,
    pub merge_commit_sha: Option<String>,
}

native_operation!(crate::backend::db::postgres::backups::list_repositories,
    crate::backend::db::sqlite::backups::list_repositories,
    /// Every repository and gist row, oldest first.
    fn list_repositories(pool: &DbPool) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::backups::set_disk_path,
    crate::backend::db::sqlite::backups::set_disk_path,
    /// Points a repository row at its files after a restore into another directory.
    fn set_disk_path(pool: &DbPool, repo_id: i64, disk_path: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::backups::list_pull_request_heads,
    crate::backend::db::sqlite::backups::list_pull_request_heads,
    fn list_pull_request_heads(pool: &DbPool, repo_id: i64) -> Result<Vec<PullRequestHead>>);

native_operation!(crate::backend::db::postgres::backups::list_ci_revisions,
    crate::backend::db::sqlite::backups::list_ci_revisions,
    /// The commits that the repository's unfinished CI runs will check out.
    fn list_ci_revisions(pool: &DbPool, repo_id: i64) -> Result<Vec<String>>);

native_operation!(crate::backend::db::postgres::backups::is_empty,
    crate::backend::db::sqlite::backups::is_empty,
    /// Whether the database has no tables yet, so a restore cannot overwrite anything.
    fn is_empty(pool: &DbPool) -> Result<bool>);

pub async fn sqlite_snapshot_repositories(path: &std::path::Path) -> Result<Vec<Repository>> {
    super::sqlite::backups::snapshot_repositories(path).await
}

/// Keep the exporting transaction alive until pg_dump has imported and read its snapshot.
pub async fn postgres_snapshot(pool: &DbPool) -> Result<(sqlx::Transaction<'static, sqlx::Postgres>, String, Vec<Repository>)> {
    match pool {
        DbPool::Postgres(pool) => super::postgres::backups::export_snapshot(pool).await,
        DbPool::Sqlite(_) => bail!("only PostgreSQL exports snapshots"),
    }
}

/// Writes a consistent copy of a SQLite database to `path` while the server runs.
pub async fn copy_sqlite(pool: &DbPool, path: &str) -> Result<()> {
    match pool {
        DbPool::Sqlite(native) => {
            crate::backend::db::sqlite::backups::vacuum_into(native, path).await
        }
        DbPool::Postgres(_) => bail!("a PostgreSQL database is copied with pg_dump"),
    }
}
