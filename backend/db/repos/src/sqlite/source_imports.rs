use crate::source_imports::SourceImport;
use anyhow::Result;
use codeyard_database::pool::SqlitePool;
use codeyard_database::queries::source_imports::ImportRefusal;

pub async fn get(pool: &SqlitePool, repo: i64) -> Result<Option<SourceImport>> {
    Ok(
        sqlx::query_as("SELECT * FROM repository_imports WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn start(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    url: &str,
    generation: &str,
) -> Result<SourceImport> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_database::queries::sqlite::discussions::lock_admin_authority(&mut tx, actor, repo)
            .await?,
        ImportRefusal::Denied
    );
    let previous: Option<SourceImport> =
        sqlx::query_as("SELECT * FROM repository_imports WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    anyhow::ensure!(
        !previous.is_some_and(|import| import.running()),
        ImportRefusal::Running
    );
    let import = sqlx::query_as("INSERT INTO repository_imports(repo_id,generation,vcs_url,status,updated_at) VALUES($1,$2,$3,'importing',$4) ON CONFLICT(repo_id) DO UPDATE SET generation=excluded.generation,vcs_url=excluded.vcs_url,status=excluded.status,updated_at=excluded.updated_at RETURNING *")
        .bind(repo).bind(generation).bind(url).bind(chrono::Utc::now().timestamp()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(import)
}

pub async fn finish(pool: &SqlitePool, repo: i64, generation: &str, success: bool) -> Result<()> {
    sqlx::query("UPDATE repository_imports SET status=$3,updated_at=$4 WHERE repo_id=$1 AND generation=$2 AND status='importing'")
        .bind(repo).bind(generation).bind(if success { "complete" } else { "error" }).bind(chrono::Utc::now().timestamp()).execute(pool).await?;
    Ok(())
}

pub async fn cancel(pool: &SqlitePool, actor: i64, repo: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_database::queries::sqlite::discussions::lock_archive_authority(
            &mut tx, actor, repo
        )
        .await?,
        ImportRefusal::Denied
    );
    sqlx::query("DELETE FROM repository_imports WHERE repo_id=$1")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
