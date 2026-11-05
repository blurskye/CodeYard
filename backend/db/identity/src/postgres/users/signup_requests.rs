use crate::users::signup_requests::ReviewError;
use crate::users::signup_requests::SignupSummary;
use crate::users::transitions::AdministratorActor;
use anyhow::Result;
use codeyard_db_core::models::SignupRequest;
use codeyard_db_core::models::User;
use codeyard_db_core::pool::PgPool;

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

pub async fn count_signup_requests<'a>(
    pool: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
    status: &str,
) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM signup_requests WHERE status=$1")
            .bind(status)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn list_signup_requests<'a>(
    pool: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
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

pub async fn admin_pending_page(
    pool: &PgPool,
    administrator: &AdministratorActor,
    limit: i64,
    requested: usize,
) -> Result<crate::users::signup_requests::AdminSignupPage> {
    let mut tx = pool.begin().await?;
    codeyard_db_core::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[administrator.user_id],
    )
    .await?;
    require_reviewer(&mut tx, administrator).await?;
    let total = count_signup_requests(&mut *tx, "pending").await?;
    let limit = limit.clamp(1, 100);
    let page = requested.clamp(1, (total as usize).div_ceil(limit as usize).max(1));
    let requests = list_signup_requests(
        &mut *tx,
        "pending",
        limit,
        ((page - 1) * limit as usize) as i64,
        0,
    )
    .await?;
    tx.commit().await?;
    Ok(crate::users::signup_requests::AdminSignupPage {
        requests,
        total,
        page,
    })
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
    crate::postgres::users::transitions::lock_accounts(&mut tx, &[reviewer_user_id]).await?;
    crate::postgres::users::transitions::lock_namespace(&mut tx).await?;
    require_reviewer(&mut tx, &reviewer).await?;

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
    if crate::postgres::users::transitions::organization_name_exists(
        &mut tx,
        &signup_request.username,
    )
    .await?
    {
        return Err(ReviewError::Denied.into());
    }

    if crate::postgres::users::transitions::email_owned(
        &mut tx,
        signup_request.email.as_deref(),
        -1,
    )
    .await?
    {
        return Err(ReviewError::Conflict.into());
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
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'signup.approve','user',$2,$3)")
        .bind(reviewer_user_id).bind(user.id).bind(serde_json::json!({"username":user.username})).execute(&mut *tx).await?;
    queue_decision_email(&mut tx, user.email.as_deref(), &user.username, true).await?;

    tx.commit().await?;
    Ok(user)
}

pub async fn reject_signup_request(
    pool: &PgPool,
    request_id: i64,
    reviewer: impl Into<AdministratorActor>,
) -> Result<(String, Option<String>)> {
    let reviewer = reviewer.into();
    let reviewer_user_id = reviewer.user_id;
    let mut tx = pool.begin().await?;
    crate::postgres::users::transitions::lock_accounts(&mut tx, &[reviewer_user_id]).await?;
    require_reviewer(&mut tx, &reviewer).await?;
    let rejected = sqlx::query_as::<_, (String, Option<String>)>(
        "UPDATE signup_requests SET status = 'rejected', reviewed_by = $1, reviewed_at = NOW(), updated_at = NOW() WHERE id = $2 AND status = 'pending' RETURNING username, email",
    )
    .bind(reviewer_user_id)
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(request) = rejected else {
        return Err(ReviewError::Processed.into());
    };
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'signup.reject','signup_request',$2,$3)")
        .bind(reviewer_user_id).bind(request_id).bind(serde_json::json!({"username":request.0})).execute(&mut *tx).await?;
    queue_decision_email(&mut tx, request.1.as_deref(), &request.0, false).await?;
    tx.commit().await?;
    Ok(request)
}

/// Queues the decision mail in the review transaction, so a recorded decision always has it.
async fn queue_decision_email(
    tx: &mut sqlx::PgConnection,
    email: Option<&str>,
    username: &str,
    approved: bool,
) -> Result<()> {
    let Some(to) = email.map(str::trim).filter(|email| !email.is_empty()) else {
        return Ok(());
    };
    let mail = codeyard_model::mail::signup_decision(username, approved);
    crate::postgres::email::queue_email(tx, to, &mail.subject, &mail.body, None).await
}

async fn require_reviewer(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &AdministratorActor,
) -> Result<()> {
    let configured = sqlx::query_scalar::<_, bool>(
        "SELECT setup_completed FROM node_configuration WHERE id=1 FOR UPDATE",
    )
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if !configured || !crate::postgres::users::transitions::active_admin_session(tx, actor).await? {
        return Err(ReviewError::Denied.into());
    }
    Ok(())
}
