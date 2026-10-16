use anyhow::Result;
use sqlx::SqlitePool;

/// Marks a file as viewed at this blob id, or clears the mark when `blob_sha` is None.
pub async fn set_pull_request_file_viewed(
    pool: &SqlitePool,
    pr_id: i64,
    user_id: i64,
    path: &str,
    blob_sha: Option<&str>,
) -> Result<()> {
    match blob_sha {
        Some(blob_sha) => {
            sqlx::query(
                "INSERT INTO pull_request_viewed_files (user_id, pull_request_id, path, blob_sha)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (user_id, pull_request_id, path) DO UPDATE SET blob_sha = excluded.blob_sha",
            )
            .bind(user_id)
            .bind(pr_id)
            .bind(path)
            .bind(blob_sha)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "DELETE FROM pull_request_viewed_files WHERE user_id = ?1 AND pull_request_id = ?2 AND path = ?3",
            )
            .bind(user_id)
            .bind(pr_id)
            .bind(path)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// The viewed marks of one reader on one pull request: each path with the blob id it was marked at.
pub async fn list_pull_request_viewed_files(
    pool: &SqlitePool,
    pr_id: i64,
    user_id: i64,
) -> Result<Vec<(String, String)>> {
    let marks: Vec<(String, String)> = sqlx::query_as(
        "SELECT path, blob_sha FROM pull_request_viewed_files WHERE user_id = ?1 AND pull_request_id = ?2",
    )
    .bind(user_id)
    .bind(pr_id)
    .fetch_all(pool)
    .await?;
    Ok(marks)
}
