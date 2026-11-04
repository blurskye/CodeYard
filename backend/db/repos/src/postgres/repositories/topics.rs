use crate::repositories::records::RepositorySettingsRefusal as Refusal;
use anyhow::Result;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;
use std::collections::BTreeMap;

pub async fn update_about(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    description: &str,
    website: Option<&str>,
    topics: &[String],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    // The repository row is locked exclusively from the start, as the UPDATE below needs it.
    anyhow::ensure!(
        crate::postgres::discussions::lock_exclusive_write_authority(&mut tx, actor_id, repo_id)
            .await?,
        Refusal::Denied
    );
    let result = sqlx::query(
        "UPDATE repositories SET description = $1, website = $2, updated_at = NOW() WHERE id = $3 AND NOT is_archived",
    )
    .bind(description)
    .bind(website)
    .bind(repo_id)
    .execute(&mut *tx)
    .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Denied);
    write_topics(&mut tx, repo_id, topics).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn repository_topics(
    pool: &PgPool,
    repo_ids: &[i64],
) -> Result<BTreeMap<i64, Vec<String>>> {
    if repo_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT repo_id, topic FROM repository_topics WHERE repo_id = ANY($1) ORDER BY repo_id, topic",
    )
    .bind(repo_ids)
    .fetch_all(pool)
    .await?;
    let mut topics: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for (repo_id, topic) in rows {
        topics.entry(repo_id).or_default().push(topic);
    }
    Ok(topics)
}

/// Replaces only the topics. The description and website keep their stored values, so a
/// concurrent edit to them is not overwritten.
pub async fn replace_topics(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    topics: &[String],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::postgres::discussions::lock_exclusive_write_authority(&mut tx, actor_id, repo_id)
            .await?,
        Refusal::Denied
    );
    let result =
        sqlx::query("UPDATE repositories SET updated_at = NOW() WHERE id = $1 AND NOT is_archived")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Denied);
    write_topics(&mut tx, repo_id, topics).await?;
    tx.commit().await?;
    Ok(())
}

async fn write_topics(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    topics: &[String],
) -> Result<()> {
    sqlx::query("DELETE FROM repository_topics WHERE repo_id = $1")
        .bind(repo_id)
        .execute(&mut **tx)
        .await?;
    for topic in topics {
        sqlx::query("INSERT INTO repository_topics (repo_id, topic) VALUES ($1, $2)")
            .bind(repo_id)
            .bind(topic)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
