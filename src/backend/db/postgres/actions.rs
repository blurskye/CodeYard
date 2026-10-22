use crate::backend::db::actions::{ActionsSettings, RepoActionsSettings};
use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
use anyhow::Result;
use sqlx::PgPool;

pub async fn get_actions_settings(pool: &PgPool) -> Result<ActionsSettings> {
    let row = sqlx::query_as(
        "SELECT enabled, run_timeout_minutes, job_cpus, job_memory_gib, runner_image
         FROM actions_settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

pub async fn save_actions_settings(
    pool: &PgPool,
    actor: i64,
    session: &str,
    settings: &ActionsSettings,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    if !super::users::transitions::active_admin(&mut tx, actor).await?
        || !super::users::transitions::live_session(&mut tx, actor, session).await?
    {
        return Ok(false);
    }
    sqlx::query(
        r#"
        INSERT INTO actions_settings (id, enabled, run_timeout_minutes, job_cpus, job_memory_gib, runner_image)
        VALUES (1, $1, $2, $3, $4, $5)
        ON CONFLICT (id) DO UPDATE SET
            enabled = EXCLUDED.enabled,
            run_timeout_minutes = EXCLUDED.run_timeout_minutes,
            job_cpus = EXCLUDED.job_cpus,
            job_memory_gib = EXCLUDED.job_memory_gib,
            runner_image = EXCLUDED.runner_image,
            updated_at = NOW()
        "#,
    )
    .bind(settings.enabled)
    .bind(settings.run_timeout_minutes)
    .bind(settings.job_cpus)
    .bind(settings.job_memory_gib)
    .bind(settings.runner_image.as_deref())
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'node.actions_settings','actions_settings',1,$2)")
        .bind(actor)
        .bind(serde_json::json!({"enabled": settings.enabled}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn get_repo_actions_settings(pool: &PgPool, repo_id: i64) -> Result<RepoActionsSettings> {
    let row = sqlx::query_as(
        "SELECT enabled, run_timeout_minutes, job_cpus, job_memory_gib
         FROM repo_actions_settings WHERE repo_id = $1",
    )
    .bind(repo_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

pub async fn save_repo_actions_settings(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    settings: &RepoActionsSettings,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id).await?,
        Refusal::Denied
    );
    let result = sqlx::query(
        r#"
        INSERT INTO repo_actions_settings (repo_id, enabled, run_timeout_minutes, job_cpus, job_memory_gib)
        SELECT id, $2, $3, $4, $5 FROM repositories WHERE id = $1 AND NOT is_archived
        ON CONFLICT (repo_id) DO UPDATE SET
            enabled = EXCLUDED.enabled,
            run_timeout_minutes = EXCLUDED.run_timeout_minutes,
            job_cpus = EXCLUDED.job_cpus,
            job_memory_gib = EXCLUDED.job_memory_gib,
            updated_at = NOW()
        "#,
    )
    .bind(repo_id)
    .bind(settings.enabled)
    .bind(settings.run_timeout_minutes)
    .bind(settings.job_cpus)
    .bind(settings.job_memory_gib)
    .execute(&mut *tx)
    .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Denied);
    tx.commit().await?;
    Ok(())
}
