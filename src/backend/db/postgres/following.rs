use crate::backend::db::models::Follow;
use anyhow::Result;
use sqlx::PgPool;

pub async fn list_connections(
    pool: &PgPool,
    user_id: i64,
    followers: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::backend::db::models::User>> {
    Ok(sqlx::query_as("SELECT u.* FROM users u JOIN follows f ON u.id=CASE WHEN $2 THEN f.follower_id ELSE f.following_id END WHERE CASE WHEN $2 THEN f.following_id ELSE f.follower_id END=$1 ORDER BY u.id LIMIT $3 OFFSET $4")
        .bind(user_id).bind(followers).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn follow_user(pool: &PgPool, follower_id: i64, following_id: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::require_account(&mut tx, follower_id, credential).await?;
    anyhow::ensure!(
        !super::users::blocks::interaction_blocked(&mut tx, follower_id, following_id).await?,
        crate::backend::db::following::FollowRestricted
    );
    let result = sqlx::query(
        "INSERT INTO follows (follower_id, following_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(follower_id)
    .bind(following_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn unfollow_user(pool: &PgPool, follower_id: i64, following_id: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::require_account(&mut tx, follower_id, credential).await?;
    let result = sqlx::query("DELETE FROM follows WHERE follower_id = $1 AND following_id = $2")
        .bind(follower_id)
        .bind(following_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn is_user_following(pool: &PgPool, follower_id: i64, following_id: i64) -> Result<bool> {
    let follow = sqlx::query_as::<_, Follow>(
        "SELECT * FROM follows WHERE follower_id = $1 AND following_id = $2",
    )
    .bind(follower_id)
    .bind(following_id)
    .fetch_optional(pool)
    .await?;

    Ok(follow.is_some())
}

pub async fn count_user_followers(pool: &PgPool, user_id: i64) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM follows
        WHERE following_id = $1
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(count)
}

pub async fn count_user_following(pool: &PgPool, user_id: i64) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM follows
        WHERE follower_id = $1
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(count)
}
