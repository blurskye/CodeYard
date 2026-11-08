use anyhow::bail;
use anyhow::Result;
use codeyard_database::models::Repository;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

/// The recorded revisions of one pull request.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PullRequestHead {
    pub id: i64,
    pub number: i64,
    pub state: String,
    pub head_sha: String,
    pub head_repo_id: Option<i64>,
    pub head_branch: String,
    pub base_sha: String,
    pub base_branch: String,
    pub fork_sha: Option<String>,
    pub merge_commit_sha: Option<String>,
}

native_operation!(crate::postgres::backups::list_repositories,
    crate::sqlite::backups::list_repositories,
    /// Every repository and gist row, oldest first.
    fn list_repositories(pool: &DbPool) -> Result<Vec<Repository>>);

native_operation!(crate::postgres::backups::set_disk_path,
    crate::sqlite::backups::set_disk_path,
    /// Points a repository row at its files after a restore into another directory.
    fn set_disk_path(pool: &DbPool, repo_id: i64, disk_path: &str) -> Result<()>);

native_operation!(crate::postgres::backups::list_pull_request_heads,
    crate::sqlite::backups::list_pull_request_heads,
    fn list_pull_request_heads(pool: &DbPool, repo_id: i64) -> Result<Vec<PullRequestHead>>);

native_operation!(crate::postgres::backups::list_ci_revisions,
    crate::sqlite::backups::list_ci_revisions,
    /// The commits that the repository's unfinished CI runs will check out.
    fn list_ci_revisions(pool: &DbPool, repo_id: i64) -> Result<Vec<String>>);

native_operation!(crate::postgres::backups::is_empty,
    crate::sqlite::backups::is_empty,
    /// Whether the database has no tables yet, so a restore cannot overwrite anything.
    fn is_empty(pool: &DbPool) -> Result<bool>);

pub async fn sqlite_snapshot_repositories(path: &std::path::Path) -> Result<Vec<Repository>> {
    let rows = crate::sqlite::backups::snapshot_repositories(path).await?;
    anyhow::ensure!(
        rows.len() as i64 <= entry_limit()?,
        "backup repository limit exceeded"
    );
    Ok(rows)
}

/// Keep the exporting transaction alive until pg_dump has imported and read its snapshot.
pub async fn postgres_snapshot(
    pool: &DbPool,
) -> Result<(
    sqlx::Transaction<'static, sqlx::Postgres>,
    String,
    Vec<Repository>,
    Vec<crate::objects::Object>,
    Vec<(i64, String)>,
)> {
    match pool {
        DbPool::Postgres(pool) => crate::postgres::backups::export_snapshot(pool).await,
        DbPool::Sqlite(_) => bail!("only PostgreSQL exports snapshots"),
    }
}

/// Writes a consistent copy of a SQLite database to `path` while the server runs.
pub async fn copy_sqlite(pool: &DbPool, path: &str) -> Result<()> {
    match pool {
        DbPool::Sqlite(native) => crate::sqlite::backups::vacuum_into(native, path).await,
        DbPool::Postgres(_) => {
            bail!("a PostgreSQL database is copied with pg_dump")
        }
    }
}

/// A bounded snapshot catalogue; it can be raised for a larger installation.
pub fn entry_limit() -> Result<i64> {
    count_limit("BACKUP_MAX_ENTRIES", 10_000, 1_000_000)
}

fn count_limit(key: &str, default: i64, maximum: i64) -> Result<i64> {
    let count = match std::env::var(key) {
        Ok(raw) => raw.parse::<i64>()?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!((1..=maximum).contains(&count), "invalid {key}");
    Ok(count)
}
native_operation!(crate::postgres::backups::restore_refs,crate::sqlite::backups::restore_refs,
    /// Operator-only, while the installation is locked offline; keeps every voter fenced.
    fn restore_refs(pool:&DbPool,repository:i64,refs:&[codeyard_database::models::RefValue],force:bool)->Result<()>);
native_operation!(crate::postgres::backups::restore_node_ready,crate::sqlite::backups::restore_node_ready,
    fn restore_node_ready(pool:&DbPool,node:i64,key:&str)->Result<()>);

native_operation!(crate::postgres::backups::node_objects,crate::sqlite::backups::node_objects,
    fn node_objects(pool:&DbPool,node:i64)->Result<Vec<crate::objects::Object>>);

native_operation!(crate::postgres::backups::restore_pending,crate::sqlite::backups::restore_pending,
    fn restore_pending(pool:&DbPool)->Result<bool>);
native_operation!(crate::postgres::backups::finish_restore,crate::sqlite::backups::finish_restore,
    fn finish_restore(pool:&DbPool)->Result<()>);

native_operation!(crate::postgres::backups::schema_name,crate::sqlite::backups::schema_name,
    fn schema_name(pool:&DbPool)->Result<String>);

native_operation!(crate::postgres::backups::restore_digest,crate::sqlite::backups::restore_digest,
    fn restore_digest(pool:&DbPool)->Result<Option<String>>);

pub fn reference_limit() -> Result<i64> {
    count_limit("BACKUP_MAX_REFERENCES", 1_000_000, 10_000_000)
}
