use crate::backend::db::actions::ActionsSettings;
use crate::backend::db::actions::RepoActionsSettings;
use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
use anyhow::Result;
use sqlx::PgPool;

pub async fn set_cache_limit(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    kind: crate::backend::db::actions::CacheLimit,
    value: i32,
) -> Result<()> {
    use crate::backend::db::actions::CacheLimit;
    use crate::backend::db::actions::CachePolicyError;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_admin_authority(&mut tx, actor, repo)
            .await?,
        CachePolicyError::Denied
    );
    let column = kind.column();
    let maximum: i32 = sqlx::query_scalar(&format!(
        "SELECT LEAST(COALESCE((SELECT {column} FROM actions_settings WHERE id=1 FOR SHARE),{}),COALESCE((SELECT s.{column} FROM org_actions_settings s JOIN repositories r ON r.owner_org_id=s.org_id AND r.owner_kind='org' WHERE r.id=$1),2147483647))",
        kind.default_value()
    ))
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(value > 0 && value <= maximum, CachePolicyError::Limit);
    sqlx::query(&format!("INSERT INTO repo_actions_settings(repo_id,{column}) VALUES($1,$2) ON CONFLICT(repo_id) DO UPDATE SET {column}=EXCLUDED.{column},updated_at=NOW()"))
        .bind(repo).bind(value).execute(&mut *tx).await?;
    match kind {
        CacheLimit::Storage => {
            crate::backend::db::postgres::action_objects::enforce_cache_limit(
                &mut tx,
                repo,
                i64::from(value) * 1024 * 1024 * 1024,
            )
            .await?
        }
        CacheLimit::Retention => {
            sqlx::query("UPDATE actions_objects SET expires_at=LEAST(expires_at,accessed_at+$2) WHERE repo_id=$1 AND kind='cache'").bind(repo).bind(i64::from(value)*86400).execute(&mut *tx).await?;
        }
    }
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'repository.actions_cache_policy','repository',$2,$3)")
        .bind(actor).bind(repo).bind(serde_json::json!({"setting":column,"value":value})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn record_ci_engine(
    pool: &PgPool,
    run_id: i64,
    worker: &str,
    engine: &str,
) -> Result<String> {
    Ok(sqlx::query_scalar("UPDATE ci_runs SET engine=COALESCE(engine,$3) WHERE id=$1 AND worker=$2 AND status='running' RETURNING engine").bind(run_id).bind(worker).bind(engine).fetch_one(pool).await?)
}

pub async fn get_actions_settings<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
) -> Result<ActionsSettings> {
    let row = sqlx::query_as(
        "SELECT default_engine, codeyard_actions_enabled, act_enabled, default_workflow_permissions, enabled, run_timeout_minutes, job_cpus, job_memory_gib, runner_image, results_url, cache_size_gib, cache_retention_days, artifact_retention_days
         FROM actions_settings WHERE id = 1",
    )
    .fetch_optional(executor)
    .await?;
    Ok(row.unwrap_or_default())
}

pub async fn save_actions_settings(
    pool: &PgPool,
    actor: i64,
    session: &str,
    settings: &ActionsSettings,
) -> Result<bool> {
    settings.validate_engines()?;
    let mut tx = pool.begin().await?;
    crate::backend::db::postgres::action_objects::lock_site_cache_policy(&mut tx, true).await?;
    crate::backend::db::postgres::users::transitions::lock_accounts_shared(&mut tx, &[actor])
        .await?;
    if !crate::backend::db::postgres::users::transitions::active_admin(&mut tx, actor).await?
        || !crate::backend::db::postgres::users::transitions::live_session(&mut tx, actor, session)
            .await?
    {
        return Ok(false);
    }
    let previous = get_actions_settings(&mut *tx).await?;
    sqlx::query(
        r#"
        INSERT INTO actions_settings (id, enabled, run_timeout_minutes, job_cpus, job_memory_gib, runner_image, results_url, cache_size_gib, cache_retention_days, artifact_retention_days, default_workflow_permissions, default_engine, codeyard_actions_enabled, act_enabled)
        VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (id) DO UPDATE SET
            enabled = EXCLUDED.enabled,
            run_timeout_minutes = EXCLUDED.run_timeout_minutes,
            job_cpus = EXCLUDED.job_cpus,
            job_memory_gib = EXCLUDED.job_memory_gib,
            runner_image = EXCLUDED.runner_image,
            results_url = EXCLUDED.results_url,
            cache_size_gib = EXCLUDED.cache_size_gib,
            cache_retention_days = EXCLUDED.cache_retention_days,
            artifact_retention_days = EXCLUDED.artifact_retention_days,
            default_workflow_permissions = EXCLUDED.default_workflow_permissions,
            default_engine = EXCLUDED.default_engine,
            codeyard_actions_enabled = EXCLUDED.codeyard_actions_enabled,
            act_enabled = EXCLUDED.act_enabled,
            updated_at = NOW()
        "#,
    )
    .bind(settings.enabled)
    .bind(settings.run_timeout_minutes)
    .bind(settings.job_cpus)
    .bind(settings.job_memory_gib)
    .bind(settings.runner_image.as_deref())
    .bind(settings.results_url.as_deref())
    .bind(settings.cache_size_gib)
    .bind(settings.cache_retention_days)
    .bind(settings.artifact_retention_days)
    .bind(&settings.default_workflow_permissions)
    .bind(&settings.default_engine)
    .bind(settings.codeyard_actions_enabled)
    .bind(settings.act_enabled)
    .execute(&mut *tx)
    .await?;
    if settings.tightens_cache_limits(&previous) {
        // Cache creation/finalization cannot change this set while the policy fence is held.
        let cached: Vec<i64> = sqlx::query_scalar("SELECT r.id FROM repositories r WHERE EXISTS(SELECT 1 FROM actions_objects o WHERE o.repo_id=r.id AND o.kind='cache') ORDER BY r.id FOR UPDATE OF r")
            .fetch_all(&mut *tx).await?;
        for repo in cached {
            crate::backend::db::postgres::action_objects::apply_cache_policy(&mut tx, repo).await?;
        }
    }
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
        "SELECT r.actions_engine AS engine, COALESCE(s.enabled,true) AS enabled, s.run_timeout_minutes, s.job_cpus, s.job_memory_gib, s.cache_size_gib, s.cache_retention_days, o.cache_size_gib AS org_cache_size_gib, o.cache_retention_days AS org_cache_retention_days
         FROM repositories r LEFT JOIN repo_actions_settings s ON s.repo_id=r.id LEFT JOIN org_actions_settings o ON o.org_id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=$1",
    )
    .bind(repo_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

pub async fn set_repo_enabled(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo: i64,
    enabled: bool,
) -> Result<()> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
            &mut tx,
            actor.user_id,
            repo
        )
        .await?
            && crate::backend::db::postgres::credentials::repository(
                &mut tx,
                &actor,
                repo,
                "administration",
                true
            )
            .await?,
        Refusal::Denied
    );
    sqlx::query("INSERT INTO repo_actions_settings(repo_id,enabled) VALUES($1,$2) ON CONFLICT(repo_id) DO UPDATE SET enabled=EXCLUDED.enabled,updated_at=NOW()")
        .bind(repo)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn save_repo_actions_settings(
    pool: &PgPool,
    actor_id: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    settings: &RepoActionsSettings,
) -> Result<()> {
    let actor = actor_id.into();
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
            &mut tx,
            actor.user_id,
            repo_id
        )
        .await?
            && crate::backend::db::postgres::credentials::repository(
                &mut tx,
                &actor,
                repo_id,
                "administration",
                true
            )
            .await?,
        Refusal::Denied
    );
    let result = sqlx::query(
        r#"
        INSERT INTO repo_actions_settings (repo_id, enabled, run_timeout_minutes, job_cpus, job_memory_gib, cache_size_gib, cache_retention_days)
        SELECT id, $2, $3, $4, $5, $6, $7 FROM repositories WHERE id = $1 AND NOT is_archived
        ON CONFLICT (repo_id) DO UPDATE SET
            enabled = EXCLUDED.enabled,
            run_timeout_minutes = EXCLUDED.run_timeout_minutes,
            job_cpus = EXCLUDED.job_cpus,
            job_memory_gib = EXCLUDED.job_memory_gib,
            cache_size_gib = EXCLUDED.cache_size_gib,
            cache_retention_days = EXCLUDED.cache_retention_days,
            updated_at = NOW()
        "#,
    )
    .bind(repo_id)
    .bind(settings.enabled)
    .bind(settings.run_timeout_minutes)
    .bind(settings.job_cpus)
    .bind(settings.job_memory_gib)
    .bind(settings.cache_size_gib)
    .bind(settings.cache_retention_days)
    .execute(&mut *tx)
    .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Denied);
    let enabled: bool = sqlx::query_scalar("SELECT COALESCE((SELECT CASE $1::text WHEN 'act' THEN act_enabled WHEN 'codeyard-actions' THEN codeyard_actions_enabled ELSE false END FROM actions_settings WHERE id=1),true)").bind(settings.engine.as_deref()).fetch_one(&mut *tx).await?;
    anyhow::ensure!(settings.engine.is_none() || enabled, Refusal::Denied);
    sqlx::query("UPDATE repositories SET actions_engine=$2 WHERE id=$1")
        .bind(repo_id)
        .bind(settings.engine.as_deref())
        .execute(&mut *tx)
        .await?;
    // New cache limits apply to the caches already stored, as the cache-policy endpoints do.
    crate::backend::db::postgres::action_objects::apply_cache_policy(&mut tx, repo_id).await?;
    tx.commit().await?;
    Ok(())
}
