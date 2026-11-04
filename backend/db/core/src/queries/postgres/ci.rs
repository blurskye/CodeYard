use crate::ci_policy::ACCESS_LOST_MESSAGE;
use crate::models::CiRun;
use anyhow::ensure;
use anyhow::Result;
use sqlx::Postgres;
use sqlx::Transaction;
pub fn is_deployment(run: &CiRun) -> bool {
    matches!(run.pipeline.as_str(), "cd" | "full") || run.environment.is_some()
}

pub async fn ci_commit_status(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run: &CiRun,
    state: &str,
    description: &str,
) -> Result<()> {
    let context = run.commit_status_context();
    let sha = run.head_sha.as_deref().map(str::to_ascii_lowercase);
    // Older attempts cannot overwrite a newer admitted attempt for this SHA/context.
    sqlx::query(r#"INSERT INTO commit_statuses(repo_id, sha, context, state, description, creator_id, ci_run_id)
        SELECT $1,$2,$3,$4,$5,$6,$7 WHERE $2 IS NOT NULL
        ON CONFLICT(repo_id,sha,context) DO UPDATE SET state = EXCLUDED.state,
            description = EXCLUDED.description, creator_id = EXCLUDED.creator_id,
            ci_run_id = EXCLUDED.ci_run_id, updated_at = NOW()
        WHERE commit_statuses.ci_run_id IS NULL OR commit_statuses.ci_run_id <= EXCLUDED.ci_run_id"#)
        .bind(run.repo_id).bind(sha).bind(context).bind(state).bind(description)
        .bind(run.triggered_by_id).bind(run.id).execute(&mut **tx).await?;
    Ok(())
}

/// Locks a run's repository before any run is locked. Revocation and deletion lock
/// the repository and then need its runs, so no transaction may hold a run while
/// it waits for the repository. Key share is the weakest lock that still waits for
/// those exclusive locks, and it is all a run's foreign keys need.
pub async fn lock_run_repository(tx: &mut Transaction<'_, Postgres>, repo_id: i64) -> Result<()> {
    let _: Option<i64> =
        sqlx::query_scalar("SELECT id FROM repositories WHERE id = $1 FOR KEY SHARE")
            .bind(repo_id)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(())
}

/// Moves a queued or running run to a final state and records the outcome: the
/// run, its deployment, the commit status and, for a failed run, the starter's
/// notification, all in the caller's transaction. The caller holds the run's row lock. A run that is
/// already finished stays as it is and the result is `None`: the first final
/// state wins.
pub async fn finish_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    run: &CiRun,
    status: &str,
    message: Option<&str>,
) -> Result<Option<(CiRun, Vec<(i64, i64)>, Vec<i64>)>> {
    ensure!(
        matches!(status, "success" | "failed" | "cancelled"),
        "invalid CI terminal status"
    );
    let finished = sqlx::query_as::<_, CiRun>(
        "UPDATE ci_runs SET status = $2, error_message = $3, completed_at = NOW(), updated_at = NOW()
         WHERE id = $1 AND status IN ('queued', 'running') RETURNING *",
    )
    .bind(run.id)
    .bind(status)
    .bind(message)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(finished) = finished else {
        return Ok(None);
    };
    crate::queries::postgres::action_logs::retain_completed(tx, run.repo_id, run.id).await?;
    let closed_jobs = crate::queries::postgres::action_jobs::close_active(tx, run.id).await?;
    crate::queries::postgres::runners::retire(tx, run.worker.as_deref()).await?;
    if is_deployment(run) {
        sqlx::query("UPDATE ci_deployments SET status = $2, completed_at = NOW(), updated_at = NOW() WHERE run_id = $1 AND status IN ('pending','in_progress','approved')")
            .bind(run.id)
            .bind(status)
            .execute(&mut **tx)
            .await?;
    }
    crate::queries::postgres::deployment_reviews::sync_execution(tx, run.id, status).await?;
    let state = if status == "success" {
        "success"
    } else {
        "failure"
    };
    let description = crate::ci_messages::visible_run_error(message);
    ci_commit_status(
        tx,
        run,
        state,
        description.as_deref().unwrap_or("CI run completed"),
    )
    .await?;
    // Only a failed run tells its starter. Successes and cancellations stay quiet.
    let notifications = if let Some(actor) = run.triggered_by_id.filter(|_| status == "failed") {
        let (title, body) = crate::ci_messages::failure_notification(run, message);
        sqlx::query_as::<_, (i64, i64)>(r#"INSERT INTO notifications(user_id, repo_id, scope_repository_id, kind, title, body, link, metadata, created_at)
            SELECT $1, r.id, r.id, 'ci_completed', $2, $3,
                '/' || COALESCE(u.username, o.name) || '/' || r.name || '/actions?job=' || $4,
                jsonb_build_object('run_id', $4::TEXT, 'status', $5::TEXT), clock_timestamp()
            FROM repositories r LEFT JOIN users u ON r.owner_user_id = u.id AND r.owner_kind = 'user'
            LEFT JOIN organizations o ON r.owner_org_id = o.id AND r.owner_kind = 'org' WHERE r.id = $6
            AND NOT EXISTS (SELECT 1 FROM watches w WHERE w.user_id = $1 AND w.repo_id = r.id AND w.level = 'ignoring')
            RETURNING user_id, id"#)
            .bind(actor)
            .bind(title)
            .bind(body)
            .bind(run.run_id.to_string())
            .bind(status)
            .bind(run.repo_id)
            .fetch_all(&mut **tx)
            .await?
    } else {
        Vec::new()
    };
    let mut threaded = Vec::with_capacity(notifications.len());
    for (user, id) in notifications {
        if crate::queries::postgres::notifications::records::threads::attach_existing(tx, id)
            .await?
        {
            threaded.push((user, id));
        }
    }
    Ok(Some((finished, threaded, closed_jobs)))
}

/// Stops the CI runs in `run_ids` after the people who started them lost access,
/// in the transaction that took the access away. Queued runs are cancelled on the
/// spot, because no worker would ever pick them up again; running ones are asked
/// to stop and the worker cancels them at its next heartbeat. Callers also pass runs
/// that finished within the last hour, the longest a blob grant lives: they keep their
/// result but lose their runtime tokens and blob grants.
pub async fn stop_ci_runs(tx: &mut Transaction<'_, Postgres>, run_ids: &[i64]) -> Result<()> {
    if run_ids.is_empty() {
        return Ok(());
    }
    // Repositories before runs, in id order (see `lock_run_repository`), so no run
    // is held while the repository of another run is awaited.
    let _: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE id IN (SELECT repo_id FROM ci_runs WHERE id = ANY($1)) ORDER BY id FOR KEY SHARE",
    )
    .bind(run_ids)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query("SELECT id FROM ci_runs WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(run_ids)
        .fetch_all(&mut **tx)
        .await?;
    let queued = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE id = ANY($1) AND status = 'queued' ORDER BY id FOR UPDATE",
    )
    .bind(run_ids)
    .fetch_all(&mut **tx)
    .await?;
    for run in &queued {
        finish_in_tx(tx, run, "cancelled", Some(ACCESS_LOST_MESSAGE)).await?;
    }
    // Revoke job PATs as well as runtime capabilities in this authority transition.
    sqlx::query("DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM actions_github_tokens WHERE run_id = ANY($1))")
        .bind(run_ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM actions_runtime_tokens WHERE run_id = ANY($1)")
        .bind(run_ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM actions_blob_grants WHERE run_id = ANY($1)")
        .bind(run_ids)
        .execute(&mut **tx)
        .await?;
    // Looked at after the queued runs are settled, so a run a worker claimed in
    // the meantime is flagged here rather than missed.
    sqlx::query("UPDATE ci_runs SET cancel_requested = TRUE, updated_at = NOW() WHERE id = ANY($1) AND status = 'running'")
        .bind(run_ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Stops queued and running work in the repositories of these organizations.
pub async fn stop_organization_ci_runs(
    tx: &mut Transaction<'_, Postgres>,
    organization_ids: &[i64],
) -> Result<()> {
    if organization_ids.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT c.id FROM ci_runs c JOIN repositories r ON r.id=c.repo_id WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=ANY($1) AND (c.status IN ('queued','running') OR c.completed_at > NOW() - INTERVAL '1 hour') ORDER BY c.id",
    )
    .bind(organization_ids)
    .fetch_all(&mut **tx)
    .await?;
    stop_ci_runs(tx, &ids).await
}

/// Stops everything `actor` has queued or running, for a ban or a forced
/// password change.
pub async fn stop_actor_ci_runs(tx: &mut Transaction<'_, Postgres>, actor: i64) -> Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM ci_runs WHERE triggered_by_id = $1 AND (status IN ('queued', 'running') OR completed_at > NOW() - INTERVAL '1 hour')",
    )
    .bind(actor)
    .fetch_all(&mut **tx)
    .await?;
    stop_ci_runs(tx, &ids).await
}

/// Stops what `actor` has queued or running in one repository, for a removed
/// collaborator.
pub async fn stop_actor_repo_ci_runs(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    repo_id: i64,
) -> Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM ci_runs WHERE triggered_by_id = $1 AND repo_id = $2 AND (status IN ('queued', 'running') OR completed_at > NOW() - INTERVAL '1 hour')",
    )
    .bind(actor)
    .bind(repo_id)
    .fetch_all(&mut **tx)
    .await?;
    stop_ci_runs(tx, &ids).await
}
