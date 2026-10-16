use crate::backend::db::models::{Star, User, Watch};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn watch_repo(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO watches (user_id, repo_id, level)
        VALUES (?1, ?2, 'watching')
        ON CONFLICT (user_id, repo_id)
        DO UPDATE SET level = 'watching'
        WHERE watches.level <> 'watching'
        "#,
    )
    .bind(user_id)
    .bind(repo_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn unwatch_repo(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM watches WHERE user_id = ?1 AND repo_id = ?2")
        .bind(user_id)
        .bind(repo_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn is_repo_watched(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let watch = sqlx::query_as::<_, Watch>(
        "SELECT * FROM watches WHERE user_id = ?1 AND repo_id = ?2 AND level != 'ignoring'",
    )
    .bind(user_id)
    .bind(repo_id)
    .fetch_optional(pool)
    .await?;

    Ok(watch.is_some())
}

pub async fn star_repo(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let result =
        sqlx::query("INSERT INTO stars (user_id, repo_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING")
            .bind(user_id)
            .bind(repo_id)
            .execute(pool)
            .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn unstar_repo(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM stars WHERE user_id = ?1 AND repo_id = ?2")
        .bind(user_id)
        .bind(repo_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn is_repo_starred(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let star = sqlx::query_as::<_, Star>("SELECT * FROM stars WHERE user_id = ?1 AND repo_id = ?2")
        .bind(user_id)
        .bind(repo_id)
        .fetch_optional(pool)
        .await?;

    Ok(star.is_some())
}

pub async fn list_repo_stargazers(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<User>> {
    anyhow::ensure!(offset >= 0, "Page offset must be nonnegative");
    let users = sqlx::query_as::<_, User>(
        r#"
        SELECT u.* FROM users u
        INNER JOIN stars s ON u.id = s.user_id
        WHERE s.repo_id = ?1
        ORDER BY s.created_at DESC, s.user_id DESC
        LIMIT ?2 OFFSET ?3
        "#,
    )
    .bind(repo_id)
    .bind(limit.clamp(1, 100))
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(users)
}
