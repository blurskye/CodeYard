use crate::users::blocks::BlockTargetMissing;
use crate::users::blocks::InteractionBlocked;
use crate::users::blocks::InvalidBlock;
use anyhow::Result;
use codeyard_db_core::models::User;
use codeyard_db_core::pool::PgPool;
use codeyard_db_core::queries::postgres::users::blocks::repository_interaction_blocked;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use sqlx::PgConnection;

pub async fn list_blocks(pool: &PgPool, user: i64, limit: i64, offset: i64) -> Result<Vec<User>> {
    Ok(sqlx::query_as("SELECT u.* FROM users u JOIN user_blocks b ON b.blocked_id=u.id WHERE b.user_id=$1 ORDER BY u.id LIMIT $2 OFFSET $3").bind(user).bind(limit).bind(offset).fetch_all(pool).await?)
}
pub async fn is_blocked(pool: &PgPool, user: i64, target: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_blocks WHERE user_id=$1 AND blocked_id=$2)",
    )
    .bind(user)
    .bind(target)
    .fetch_one(pool)
    .await?)
}

pub async fn interaction_blocked(connection: &mut PgConnection, a: i64, b: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_blocks WHERE (user_id=$1 AND blocked_id=$2) OR (user_id=$2 AND blocked_id=$1))").bind(a).bind(b).fetch_one(connection).await?)
}

/// Call before the first repository lock. Blocks exclude both accounts until commit.
pub async fn require_repository_interaction(
    connection: &mut PgConnection,
    actor: i64,
    repo: i64,
) -> Result<()> {
    codeyard_db_core::queries::postgres::users::transitions::lock_accounts_shared(
        connection,
        &[actor],
    )
    .await?;
    let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password)").bind(actor).fetch_one(&mut *connection).await?;
    anyhow::ensure!(
        active && !repository_interaction_blocked(connection, actor, repo).await?,
        InteractionBlocked
    );
    Ok(())
}

pub async fn change_block(
    pool: &PgPool,
    user: i64,
    credential: &AccountCredential<'_>,
    target: i64,
    blocked: bool,
) -> Result<()> {
    anyhow::ensure!(user != target, InvalidBlock);
    let mut tx = pool.begin().await?;
    // Either participant's effects must finish before removing their shared state.
    crate::postgres::users::transitions::lock_accounts(&mut tx, &[user, target]).await?;
    crate::postgres::users::transitions::require_account_exclusive(&mut tx, user, credential)
        .await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(exists, BlockTargetMissing);
    if !blocked {
        sqlx::query("DELETE FROM user_blocks WHERE user_id=$1 AND blocked_id=$2")
            .bind(user)
            .bind(target)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }
    let inserted = sqlx::query(
        "INSERT INTO user_blocks(user_id,blocked_id) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(user)
    .bind(target)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        tx.commit().await?;
        return Ok(());
    }
    sqlx::query("DELETE FROM follows WHERE (follower_id=$1 AND following_id=$2) OR (follower_id=$2 AND following_id=$1)").bind(user).bind(target).execute(&mut *tx).await?;
    let repositories = "SELECT id FROM repositories WHERE owner_kind='user' AND owner_user_id=$1";
    for (owner, actor) in [(user, target), (target, user)] {
        let _: Vec<i64> = sqlx::query_scalar(&format!("SELECT id FROM repositories WHERE id IN ({repositories}) AND (id IN (SELECT repo_id FROM stars WHERE user_id=$2) OR id IN (SELECT repo_id FROM watches WHERE user_id=$2) OR id IN (SELECT repo_id FROM gist_stars WHERE user_id=$2)) ORDER BY id FOR UPDATE"))
            .bind(owner).bind(actor).fetch_all(&mut *tx).await?;
        for table in ["stars", "watches", "repo_access", "gist_stars"] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE user_id=$2 AND repo_id IN ({repositories})"
            ))
            .bind(owner)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(&format!("DELETE FROM issue_assignees WHERE user_id=$2 AND issue_id IN (SELECT id FROM issues WHERE repo_id IN ({repositories}))"))
            .bind(owner).bind(actor).execute(&mut *tx).await?;
        for table in ["pr_assignees", "pr_reviewers"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE user_id=$2 AND pull_request_id IN (SELECT id FROM pull_requests WHERE repo_id IN ({repositories}))"))
                .bind(owner).bind(actor).execute(&mut *tx).await?;
        }
        sqlx::query(&format!("UPDATE repository_invitations SET state='cancelled' WHERE invitee_id=$2 AND state='pending' AND repo_id IN ({repositories})"))
            .bind(owner).bind(actor).execute(&mut *tx).await?;
        let runs: Vec<i64> = sqlx::query_scalar(&format!("SELECT id FROM ci_runs WHERE triggered_by_id=$2 AND repo_id IN ({repositories}) AND (status IN ('queued','running') OR completed_at>clock_timestamp()-INTERVAL '1 hour')"))
            .bind(owner).bind(actor).fetch_all(&mut *tx).await?;
        codeyard_db_core::queries::postgres::ci::stop_ci_runs(&mut tx, &runs).await?;
    }
    sqlx::query("DELETE FROM notifications WHERE (user_id=$1 AND actor_id=$2) OR (user_id=$2 AND actor_id=$1)").bind(user).bind(target).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
