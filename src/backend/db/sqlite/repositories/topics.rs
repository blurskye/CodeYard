use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::BTreeMap;

pub async fn update_about(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    description: &str,
    website: Option<&str>,
    topics: &[String],
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo_id)
            .await?,
        Refusal::Denied
    );
    let result = sqlx::query(
        "UPDATE repositories SET description = ?1, website = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?3 AND NOT is_archived",
    )
    .bind(description)
    .bind(website)
    .bind(repo_id)
    .execute(&mut *tx)
    .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Denied);
    sqlx::query("DELETE FROM repository_topics WHERE repo_id = ?1")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    for topic in topics {
        sqlx::query("INSERT INTO repository_topics (repo_id, topic) VALUES (?1, ?2)")
            .bind(repo_id)
            .bind(topic)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn repository_topics(
    pool: &SqlitePool,
    repo_ids: &[i64],
) -> Result<BTreeMap<i64, Vec<String>>> {
    if repo_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT repo_id, topic FROM repository_topics WHERE repo_id IN (SELECT value FROM json_each(?1)) ORDER BY repo_id, topic",
    )
    .bind(serde_json::to_string(repo_ids)?)
    .fetch_all(pool)
    .await?;
    let mut topics: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for (repo_id, topic) in rows {
        topics.entry(repo_id).or_default().push(topic);
    }
    Ok(topics)
}
