use anyhow::Result;
use codeyard_db_core::models::Follow;
use sqlx::SqlitePool;

pub async fn list_connections(
    pool: &SqlitePool,
    user_id: i64,
    followers: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<codeyard_db_core::models::User>> {
    Ok(
        sqlx::query_as(crate::backend::db::following::connections_sql(followers))
            .bind(user_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn follow_user(
    pool: &SqlitePool,
    follower_id: i64,
    following_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::backend::db::sqlite::users::transitions::require_account(
        &mut tx,
        follower_id,
        credential,
    )
    .await?;
    anyhow::ensure!(
        !crate::backend::db::sqlite::users::blocks::interaction_blocked(
            &mut tx,
            follower_id,
            following_id
        )
        .await?,
        crate::backend::db::following::FollowRestricted
    );
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

pub async fn unfollow_user(
    pool: &SqlitePool,
    follower_id: i64,
    following_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::backend::db::sqlite::users::transitions::require_account(
        &mut tx,
        follower_id,
        credential,
    )
    .await?;
    let result = sqlx::query("DELETE FROM follows WHERE follower_id = ?1 AND following_id = ?2")
        .bind(follower_id)
        .bind(following_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
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
