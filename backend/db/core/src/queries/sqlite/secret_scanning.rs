use crate::pool::SqlitePool;
use crate::queries::secret_scanning::Refusal;
use crate::queries::secret_scanning::Settings;
use anyhow::ensure;
use anyhow::Result;
pub async fn settings(pool: &SqlitePool, repo: i64) -> Result<Settings> {
    Ok(sqlx::query_as(
        "SELECT enabled,push_protection FROM secret_scanning_settings WHERE repo_id=$1",
    )
    .bind(repo)
    .fetch_optional(pool)
    .await?
    .unwrap_or_default())
}

pub async fn save_settings_on_connection(
    connection: &mut sqlx::SqliteConnection,
    actor: Option<i64>,
    repo: i64,
    settings: &Settings,
) -> Result<()> {
    ensure!(
        !settings.push_protection || settings.enabled,
        Refusal::Invalid
    );
    let previous: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT enabled FROM secret_scanning_settings WHERE repo_id=$1),false)",
    )
    .bind(repo)
    .fetch_one(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO secret_scanning_settings(repo_id,enabled,push_protection) VALUES($1,$2,$3) ON CONFLICT(repo_id) DO UPDATE SET enabled=$2,push_protection=$3")
        .bind(repo).bind(settings.enabled).bind(settings.push_protection).execute(&mut *connection).await?;
    if settings.enabled && !previous {
        sqlx::query(
            "INSERT INTO secret_scanning_jobs(repo_id,scan_type,actor_id) VALUES($1,'backfill',$2)",
        )
        .bind(repo)
        .bind(actor)
        .execute(connection)
        .await?;
    }
    Ok(())
}
