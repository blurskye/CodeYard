use crate::backend::db::repository_insights::{ActivityCounts, NumberedItem};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

pub async fn count_activity(
    pool: &PgPool,
    repo_id: i64,
    since: DateTime<Utc>,
) -> Result<ActivityCounts> {
    Ok(sqlx::query_as::<_, ActivityCounts>(
        r#"
        SELECT
            (SELECT COUNT(*) FROM pull_requests WHERE repo_id = $1 AND merged_at >= $2) AS merged_pulls,
            (SELECT COUNT(*) FROM pull_requests WHERE repo_id = $1 AND created_at >= $2) AS opened_pulls,
            (SELECT COUNT(*) FROM issues WHERE repo_id = $1 AND closed_at >= $2) AS closed_issues,
            (SELECT COUNT(*) FROM issues WHERE repo_id = $1 AND created_at >= $2) AS opened_issues
        "#,
    )
    .bind(repo_id)
    .bind(since)
    .fetch_one(pool)
    .await?)
}

pub async fn list_merged_pulls(
    pool: &PgPool,
    repo_id: i64,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<NumberedItem>> {
    Ok(sqlx::query_as::<_, NumberedItem>(
        r#"
        SELECT number, title, merged_at AS at FROM pull_requests
        WHERE repo_id = $1 AND merged_at >= $2
        ORDER BY merged_at DESC, id DESC
        LIMIT $3
        "#,
    )
    .bind(repo_id)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn list_opened_issues(
    pool: &PgPool,
    repo_id: i64,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<NumberedItem>> {
    Ok(sqlx::query_as::<_, NumberedItem>(
        r#"
        SELECT number, title, created_at AS at FROM issues
        WHERE repo_id = $1 AND created_at >= $2
        ORDER BY created_at DESC, id DESC
        LIMIT $3
        "#,
    )
    .bind(repo_id)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
