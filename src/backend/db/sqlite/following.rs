use crate::backend::db::models::Follow;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn follow_user(pool: &SqlitePool, follower_id: i64, following_id: i64) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    require_active(&mut tx, follower_id).await?;
    let result = sqlx::query(
        "INSERT INTO follows (follower_id, following_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
    )
    .bind(follower_id)
    .bind(following_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn unfollow_user(pool: &SqlitePool, follower_id: i64, following_id: i64) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    require_active(&mut tx, follower_id).await?;
    let result = sqlx::query("DELETE FROM follows WHERE follower_id = ?1 AND following_id = ?2")
        .bind(follower_id)
        .bind(following_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

async fn require_active(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, actor: i64) -> Result<()> {
    let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password)")
        .bind(actor).fetch_one(&mut **tx).await?;
    anyhow::ensure!(active, crate::backend::db::following::FollowRestricted);
    Ok(())
}

pub async fn is_user_following(
    pool: &SqlitePool,
    follower_id: i64,
    following_id: i64,
) -> Result<bool> {
    let follow = sqlx::query_as::<_, Follow>(
        "SELECT * FROM follows WHERE follower_id = ?1 AND following_id = ?2",
    )
    .bind(follower_id)
    .bind(following_id)
    .fetch_optional(pool)
    .await?;

    Ok(follow.is_some())
}

pub async fn count_user_followers(pool: &SqlitePool, user_id: i64) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM follows
        WHERE following_id = ?1
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(count)
}

pub async fn count_user_following(pool: &SqlitePool, user_id: i64) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM follows
        WHERE follower_id = ?1
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(count)
}
