use crate::backend::db::models::{SignupRequest, User};
use crate::backend::db::users::{signup_requests::ReviewError, transitions::AdministratorActor};
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
        INSERT INTO signup_requests (username, email, password_hash, status)
        VALUES ($1, $2, $3, 'pending')
        RETURNING *
        "#,
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .fetch_one(pool)
    .await?;

    Ok(request)
}

pub async fn list_signup_requests(pool: &PgPool, status: &str) -> Result<Vec<SignupRequest>> {
    let requests = sqlx::query_as::<_, SignupRequest>(
        "SELECT * FROM signup_requests WHERE status = $1 ORDER BY created_at DESC",
    )
    .bind(status)
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
              LOWER(username) = LOWER($1)
              OR ($2 IS NOT NULL AND email IS NOT NULL AND LOWER(email) = LOWER($2))
          )
        LIMIT 1
        "#,
    )
    .bind(username)
    .bind(email)
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

    // The shared review contract has no Conflict variant: deny this approval,
    // leaving the request pending and actor/status/account changes atomic.
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
    .await?;

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
