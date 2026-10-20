use crate::backend::db::models::{SignupRequest, User};
use crate::backend::db::users::{
    signup_requests::{ReviewError, SignupSummary},
    transitions::AdministratorActor,
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn create_signup_request(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
) -> Result<SignupRequest> {
    let request = sqlx::query_as::<_, SignupRequest>(
        r#"
        INSERT INTO signup_requests (username, email, password_hash, username_key, email_key, status)
        VALUES ($1, $2, $3, $4, $5, 'pending')
        RETURNING *
        "#,
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(username.to_lowercase())
    .bind(email.map(str::to_lowercase))
    .fetch_one(pool)
    .await?;

    Ok(request)
}

pub async fn count_signup_requests(pool: &PgPool, status: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM signup_requests WHERE status=$1")
            .bind(status)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn list_signup_requests(
    pool: &PgPool,
    status: &str,
    limit: i64,
    offset: i64,
    after: i64,
) -> Result<Vec<SignupSummary>> {
    let requests = sqlx::query_as::<_, SignupSummary>(
        "SELECT id,username,email,status,created_at FROM signup_requests WHERE status = $1 AND id>$4 ORDER BY id LIMIT $2 OFFSET $3",
    )
    .bind(status)
    .bind(limit.clamp(1,100))
    .bind(offset.max(0))
    .bind(after.max(0))
    .fetch_all(pool)
    .await?;

    Ok(requests)
}

pub async fn get_pending_signup_request_by_identity(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
) -> Result<Option<SignupRequest>> {
    let request = sqlx::query_as::<_, SignupRequest>(
        r#"
        SELECT *
        FROM signup_requests
        WHERE status = 'pending'
          AND (
              username_key = $1
              OR ($2::text IS NOT NULL AND email_key = $2)
          )
        LIMIT 1
        "#,
    )
    .bind(username.to_lowercase())
    .bind(email.map(str::to_lowercase))
    .fetch_optional(pool)
    .await?;

    Ok(request)
}

pub async fn approve_signup_request(
    pool: &PgPool,
    request_id: i64,
    reviewer: impl Into<AdministratorActor>,
) -> Result<User> {
    let reviewer = reviewer.into();
    let reviewer_user_id = reviewer.user_id;
    let mut tx = pool.begin().await?;
    super::transitions::lock_membership(&mut tx).await?;
    super::transitions::lock_namespace(&mut tx).await?;
    require_private_reviewer(&mut tx, &reviewer).await?;

    let signup_request = sqlx::query_as::<_, SignupRequest>(
        "SELECT * FROM signup_requests WHERE id = $1 FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ReviewError::Missing)?;

    if signup_request.status != "pending" {
        return Err(ReviewError::Processed.into());
    }

    // Leave a conflicting request pending for an explicit rejection.
    if super::transitions::organization_name_exists(&mut tx, &signup_request.username).await? {
        return Err(ReviewError::Denied.into());
    }

    let user = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (username, email, password_hash, is_admin, must_change_password)
        VALUES ($1, $2, $3, FALSE, FALSE)
        RETURNING *
        "#,
    )
    .bind(&signup_request.username)
    .bind(signup_request.email.as_deref())
    .bind(&signup_request.password_hash)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => ReviewError::Conflict.into(),
        _ => anyhow::Error::from(error),
    })?;

    sqlx::query(
        "UPDATE signup_requests SET status = 'approved', reviewed_by = $1, reviewed_at = NOW(), updated_at = NOW() WHERE id = $2",
    )
    .bind(reviewer_user_id)
    .bind(request_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(user)
}

pub async fn reject_signup_request(
    pool: &PgPool,
    request_id: i64,
    reviewer: impl Into<AdministratorActor>,
) -> Result<()> {
    let reviewer = reviewer.into();
    let reviewer_user_id = reviewer.user_id;
    let mut tx = pool.begin().await?;
    super::transitions::lock_membership(&mut tx).await?;
    require_private_reviewer(&mut tx, &reviewer).await?;
    let result = sqlx::query(
        "UPDATE signup_requests SET status = 'rejected', reviewed_by = $1, reviewed_at = NOW(), updated_at = NOW() WHERE id = $2 AND status = 'pending'",
    )
    .bind(reviewer_user_id)
    .bind(request_id)
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ReviewError::Processed.into());
    }
    tx.commit().await?;
    Ok(())
}

async fn require_private_reviewer(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &AdministratorActor,
) -> Result<()> {
    let private = sqlx::query_scalar::<_, bool>("SELECT setup_completed AND node_mode='private' FROM node_configuration WHERE id=1 FOR UPDATE")
        .fetch_optional(&mut **tx).await?.unwrap_or(false);
    if !private || !super::transitions::active_admin_session(tx, actor).await? {
        return Err(ReviewError::Denied.into());
    }
    Ok(())
}
