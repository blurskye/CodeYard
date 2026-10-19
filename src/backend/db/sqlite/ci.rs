//! CI runs as a job queue: triggers enqueue, worker nodes claim, heartbeat and
//! finish runs.
use super::models::{CiDeployment, CiLogLine, CiRun};
use crate::backend::db::ci::{CiAdmissionRefusal, NewCiRun, ACCESS_LOST_MESSAGE, CLAIM_SCAN_LIMIT};
use anyhow::{bail, ensure, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

fn is_deployment(run: &CiRun) -> bool {
    matches!(run.pipeline.as_str(), "cd" | "full") || run.environment.is_some()
}

/// Whether `actor` may run (or, with `cd`, deploy) workflows for this repository.
/// Workflows run on the server's own machines, so besides repository access the
/// actor must be a site admin or listed in CI_TRUSTED_ACTOR_IDS.
async fn ci_actor_has_permission(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    repo_id: i64,
    cd: bool,
    trusted_ids: &[i64],
) -> Result<bool> {
    let user = sqlx::query_as::<_, super::models::User>("SELECT * FROM users WHERE id = ?1")
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(user) = user else { return Ok(false) };
    if user.is_banned
        || user.must_change_password
        || !(user.is_admin || trusted_ids.contains(&actor))
    {
        return Ok(false);
    }
    let repo =
        sqlx::query_as::<_, super::models::Repository>("SELECT * FROM repositories WHERE id = ?1")
            .bind(repo_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(repo) = repo else { return Ok(false) };
    if repo.is_archived {
        return Ok(false);
    }
    let access =
        super::repositories::access::user_repo_access_in_transaction(tx, actor, repo_id).await?;
    let personal_admin = user.is_admin
        && repo.owner_kind == "user"
        && repo.owner_user_id.is_some()
        && repo.owner_org_id.is_none();
    Ok(personal_admin
        || if cd {
            access.can_admin()
        } else {
            access.can_write()
        })
}

async fn ci_commit_status(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run: &CiRun,
    state: &str,
    description: &str,
) -> Result<()> {
    let context = run.commit_status_context();
    let sha = run.head_sha.as_deref().map(str::to_ascii_lowercase);
    // Older attempts cannot overwrite a newer admitted attempt for this SHA/context.
    sqlx::query(r#"INSERT INTO commit_statuses(repo_id, sha, context, state, description, creator_id, ci_run_id)
        SELECT ?1,?2,?3,?4,?5,?6,?7 WHERE ?2 IS NOT NULL
        ON CONFLICT(repo_id,sha,context) DO UPDATE SET state = EXCLUDED.state,
            description = EXCLUDED.description, creator_id = EXCLUDED.creator_id,
            ci_run_id = EXCLUDED.ci_run_id, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
        WHERE commit_statuses.ci_run_id IS NULL OR commit_statuses.ci_run_id <= EXCLUDED.ci_run_id"#)
        .bind(run.repo_id).bind(sha).bind(context).bind(state).bind(description)
        .bind(run.triggered_by_id).bind(run.id).execute(&mut **tx).await?;
    Ok(())
}

/// The condition for a pull request that a CI run on `branch` may name: its base is `branch`, or it
/// is a layer of a stack that lands on `branch`. A layer runs for the trunk its stack merges into,
/// which is the base of an open layer of the same stack that has no open layer below it. `branch`
/// is the placeholder of the bound branch name.
fn branch_or_trunk_sql(branch: &str) -> String {
    format!(
        "(base_branch = {branch} OR (stack_id IS NOT NULL AND EXISTS (SELECT 1 FROM pull_requests trunk_layer WHERE trunk_layer.repo_id = pull_requests.repo_id AND trunk_layer.stack_id = pull_requests.stack_id AND trunk_layer.state = 'open' AND trunk_layer.base_branch = {branch} AND NOT EXISTS (SELECT 1 FROM pull_requests below WHERE below.repo_id = trunk_layer.repo_id AND below.stack_id = trunk_layer.stack_id AND below.state = 'open' AND below.head_branch = trunk_layer.base_branch))))"
    )
}

/// Checks permissions and inserts a queued run with its pending commit status
/// (and deployment, for CD pipelines) in one transaction.
pub async fn enqueue_ci_run(
    pool: &SqlitePool,
    input: NewCiRun<'_>,
    trusted_ids: &[i64],
) -> Result<CiRun> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let cd = matches!(input.pipeline, "cd" | "full") || input.environment.is_some();
    ensure!(
        ci_actor_has_permission(
            &mut tx,
            input.triggered_by_id,
            input.repo_id,
            cd,
            trusted_ids
        )
        .await?,
        CiAdmissionRefusal::Denied("CI repository execution permission denied")
    );
    let mut is_fork = false;
    let mut pull_request_id = None;
    let mut pull_request_event = None;
    if input.event == "pull_request" {
        let id = match input.pull_request_id {
            Some(id) => id,
            None => {
                let lookup = format!(
                    "SELECT id FROM pull_requests WHERE repo_id = ?1 AND head_sha = ?2 AND {} AND state = 'open' AND NOT is_draft ORDER BY id LIMIT 2",
                    branch_or_trunk_sql("?3")
                );
                let ids: Vec<i64> = sqlx::query_scalar(&lookup)
                    .bind(input.repo_id)
                    .bind(input.head_sha)
                    .bind(input.branch)
                    .fetch_all(&mut *tx)
                    .await?;
                ensure!(
                    ids.len() == 1,
                    CiAdmissionRefusal::Conflict("CI pull request revision is not open")
                );
                ids[0]
            }
        };
        let context_sql = format!(
            "SELECT head_repo_id IS NOT repo_id, COALESCE(workflows_approved_sha = head_sha, 0), json_object('number', number, 'title', title, 'head_ref', head_branch, 'base_ref', base_branch, 'base_sha', base_sha) FROM pull_requests WHERE id = ?1 AND repo_id = ?2 AND head_sha = ?3 AND {} AND state = 'open' AND NOT is_draft",
            branch_or_trunk_sql("?4")
        );
        let context: Option<(bool, bool, String)> = sqlx::query_as(&context_sql)
            .bind(id)
            .bind(input.repo_id)
            .bind(input.head_sha)
            .bind(input.branch)
            .fetch_optional(&mut *tx)
            .await?;
        let (fork, approved, event) = context.ok_or(CiAdmissionRefusal::Conflict(
            "CI pull request revision is not open",
        ))?;
        is_fork = fork;
        pull_request_id = Some(id);
        pull_request_event = Some(event);
        ensure!(
            !fork || approved,
            CiAdmissionRefusal::Denied("Fork workflows require approval for this head commit")
        );
        if let Some(run) = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE pull_request_id = ?1 AND event = 'pull_request' AND branch = ?2 AND head_sha = ?3 AND workflow_path = ?4")
            .bind(id).bind(input.branch).bind(input.head_sha).bind(input.workflow_path).fetch_optional(&mut *tx).await? {
            return Ok(run);
        }
    }
    let run = sqlx::query_as::<_, CiRun>(
        "INSERT INTO ci_runs (run_id, repo_id, triggered_by_id, mode, pipeline, event, pull_request_id, branch, head_sha, workflow_path, environment, workflow_sha, workflow_blobs, is_fork, pull_request_event, workflow_inputs)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?9,?12,?13,?14,?15) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(input.repo_id)
    .bind(input.triggered_by_id)
    .bind("unassigned")
    .bind(input.pipeline)
    .bind(input.event)
    .bind(pull_request_id)
    .bind(input.branch)
    .bind(input.head_sha)
    .bind(input.workflow_path)
    .bind(input.environment)
    .bind(serde_json::to_string(input.workflow_blobs)?)
    .bind(is_fork)
    .bind(pull_request_event)
    .bind(input.workflow_inputs.map(serde_json::to_string).transpose()?)
    .fetch_one(&mut *tx)
    .await?;
    if cd {
        sqlx::query("INSERT INTO ci_deployments(run_id, repo_id, environment) VALUES (?1,?2,?3)")
            .bind(run.id)
            .bind(run.repo_id)
            .bind(run.environment.as_deref().unwrap_or("production"))
            .execute(&mut *tx)
            .await?;
    }
    ci_commit_status(&mut tx, &run, "pending", "CI run queued").await?;
    tx.commit().await?;
    Ok(run)
}

/// Takes the oldest queued run this worker may start, and cancels the queued
/// runs it passes whose starter has lost access since they were queued. SQLite
/// has a single writer, so no other worker or revocation can interleave. At most
/// `CLAIM_SCAN_LIMIT` runs are looked at per call.
pub async fn claim_ci_run(
    pool: &SqlitePool,
    worker: &str,
    mode: &str,
    trusted_ids: &[i64],
) -> Result<Option<CiRun>> {
    claim_queued_run(pool, None, worker, mode, trusted_ids).await
}

/// `claim_ci_run` for a cluster node. The node is read inside the write transaction.
pub async fn claim_ci_run_on_node(
    pool: &SqlitePool,
    node_key: &str,
    worker: &str,
    mode: &str,
    trusted_ids: &[i64],
) -> Result<Option<CiRun>> {
    claim_queued_run(pool, Some(node_key), worker, mode, trusted_ids).await
}

async fn claim_queued_run(
    pool: &SqlitePool,
    node_key: Option<&str>,
    worker: &str,
    mode: &str,
    trusted_ids: &[i64],
) -> Result<Option<CiRun>> {
    ensure!(
        matches!(mode, "local" | "podman"),
        "invalid CI execution mode"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(node_key) = node_key {
        // BEGIN IMMEDIATE holds the write lock until this claim commits, so a
        // revocation waits for it. Checking before any run is touched leaves every
        // run queued when the node is refused.
        let active = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM nodes WHERE public_key = ?1 AND status = 'active'
             AND EXISTS (SELECT 1 FROM json_each(nodes.roles) WHERE value = 'actions-worker')",
        )
        .bind(node_key)
        .fetch_optional(&mut *tx)
        .await?;
        if active.is_none() {
            tx.rollback().await?;
            return Ok(None);
        }
    }
    let mut after = 0_i64;
    for _ in 0..CLAIM_SCAN_LIMIT {
        let next = sqlx::query_as::<_, CiRun>(
            "SELECT * FROM ci_runs WHERE status = 'queued' AND id > ?1 ORDER BY id LIMIT 1",
        )
        .bind(after)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(run) = next else { break };
        after = run.id;
        let permitted = match run.triggered_by_id {
            Some(actor) if !run.cancel_requested => {
                ci_actor_has_permission(
                    &mut tx,
                    actor,
                    run.repo_id,
                    is_deployment(&run),
                    trusted_ids,
                )
                .await?
            }
            _ => false,
        };
        if run.cancel_requested || !permitted {
            // A queued run with a cancellation request can only be left over from
            // before such runs were cancelled on the spot.
            let message = if run.cancel_requested {
                "Cancelled before it started"
            } else {
                ACCESS_LOST_MESSAGE
            };
            finish_in_tx(&mut tx, &run, "cancelled", Some(message)).await?;
            continue;
        }
        let run = sqlx::query_as::<_, CiRun>(
            "UPDATE ci_runs SET status = 'running', worker = ?2, mode = ?3, started_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), heartbeat_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
             WHERE id = ?1 RETURNING *",
        )
        .bind(run.id)
        .bind(worker)
        .bind(mode)
        .fetch_one(&mut *tx)
        .await?;
        if is_deployment(&run) {
            sqlx::query("UPDATE ci_deployments SET status = 'in_progress', started_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE run_id = ?1 AND status = 'pending'")
                .bind(run.id)
                .execute(&mut *tx)
                .await?;
        }
        ci_commit_status(&mut tx, &run, "pending", "CI run in progress").await?;
        tx.commit().await?;
        return Ok(Some(run));
    }
    tx.commit().await?;
    Ok(None)
}

pub async fn ci_run_allows_repository_read(
    pool: &SqlitePool,
    run_id: i64,
    repo_id: i64,
    worker: &str,
    public_key: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ci_runs WHERE id = ?1 AND repo_id = ?2
         AND worker = ?3 AND status = 'running' AND NOT cancel_requested
         AND substr(worker, 1, length(?4) + 1) = ?4 || '/')",
    )
    .bind(run_id)
    .bind(repo_id)
    .bind(worker)
    .bind(public_key)
    .fetch_one(pool)
    .await?)
}

/// Records that the worker is alive. Returns whether cancellation was requested,
/// or `None` if this worker no longer owns the run.
pub async fn heartbeat_ci_run(pool: &SqlitePool, id: i64, worker: &str) -> Result<Option<bool>> {
    Ok(sqlx::query_scalar(
        "UPDATE ci_runs SET heartbeat_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?1 AND worker = ?2 AND status = 'running' RETURNING cancel_requested",
    )
    .bind(id)
    .bind(worker)
    .fetch_optional(pool)
    .await?)
}

/// Moves a queued or running run to a final state and records the outcome: the
/// run, its deployment, the commit status and, for a failed run, the starter's
/// notification, all in the caller's transaction. A run that is already finished
/// stays as it is and the result is `None`: the first final state wins.
async fn finish_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    run: &CiRun,
    status: &str,
    message: Option<&str>,
) -> Result<Option<(CiRun, Vec<(i64, i64)>)>> {
    ensure!(
        matches!(status, "success" | "failed" | "cancelled"),
        "invalid CI terminal status"
    );
    let finished = sqlx::query_as::<_, CiRun>(
        "UPDATE ci_runs SET status = ?2, error_message = ?3, completed_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE id = ?1 AND status IN ('queued', 'running') RETURNING *",
    )
    .bind(run.id)
    .bind(status)
    .bind(message)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(finished) = finished else {
        return Ok(None);
    };
    if is_deployment(run) {
        sqlx::query("UPDATE ci_deployments SET status = ?2, completed_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE run_id = ?1 AND status IN ('pending','in_progress','approved')")
            .bind(run.id)
            .bind(status)
            .execute(&mut **tx)
            .await?;
    }
    let state = if status == "success" {
        "success"
    } else {
        "failure"
    };
    ci_commit_status(tx, run, state, message.unwrap_or("CI run completed")).await?;
    // Only a failed run tells its starter. Successes and cancellations stay quiet.
    let notifications = if let Some(actor) = run.triggered_by_id.filter(|_| status == "failed") {
        let (title, body) = crate::backend::ci::failure_notification(run, message);
        sqlx::query_as::<_, (i64, i64)>(r#"INSERT INTO notifications(user_id, repo_id, scope_repository_id, kind, title, body, link, metadata)
            SELECT ?1, r.id, r.id, 'ci_completed', ?2, ?3,
                '/' || COALESCE(u.username, o.name) || '/' || r.name || '/actions?job=' || ?4,
                json_object('run_id', ?4, 'status', ?5)
            FROM repositories r LEFT JOIN users u ON r.owner_user_id = u.id AND r.owner_kind = 'user'
            LEFT JOIN organizations o ON r.owner_org_id = o.id AND r.owner_kind = 'org' WHERE r.id = ?6
            AND NOT EXISTS (SELECT 1 FROM watches w WHERE w.user_id = ?1 AND w.repo_id = r.id AND w.level = 'ignoring')
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
    Ok(Some((finished, notifications)))
}

/// Marks a running run finished, if this worker still owns it. A stale sweep or a
/// cancellation that got there earlier has already decided the outcome, and this
/// call changes nothing.
pub async fn finish_ci_run(
    pool: &SqlitePool,
    id: i64,
    worker: &str,
    status: &str,
    message: Option<&str>,
) -> Result<Option<(CiRun, Vec<(i64, i64)>)>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let run = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE id = ?1 AND worker = ?2 AND status = 'running'",
    )
    .bind(id)
    .bind(worker)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(run) = run else {
        return Ok(None);
    };
    let finished = finish_in_tx(&mut tx, &run, status, message).await?;
    tx.commit().await?;
    Ok(finished)
}

/// Fails running runs whose worker stopped sending heartbeats. Workflows are
/// never retried automatically: they may have had external effects.
pub async fn fail_stale_ci_runs(pool: &SqlitePool, stale_seconds: i64) -> Result<usize> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let runs = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE status = 'running' AND (heartbeat_at IS NULL OR heartbeat_at < strftime('%Y-%m-%dT%H:%M:%f000Z','now', '-' || ?1 || ' seconds')) ORDER BY id LIMIT 128",
    )
    .bind(stale_seconds)
    .fetch_all(&mut *tx)
    .await?;
    let mut failed = 0;
    for run in &runs {
        let message = Some("The CI worker stopped before the run finished");
        if finish_in_tx(&mut tx, run, "failed", message)
            .await?
            .is_some()
        {
            failed += 1;
        }
    }
    tx.commit().await?;
    Ok(failed)
}

/// Stops the CI runs in `run_ids` after the people who started them lost access,
/// in the transaction that took the access away. Queued runs are cancelled on the
/// spot, because no worker would ever pick them up again; running ones are asked
/// to stop and the worker cancels them at its next heartbeat. Finished runs are
/// left alone.
pub(crate) async fn stop_ci_runs(tx: &mut Transaction<'_, Sqlite>, run_ids: &[i64]) -> Result<()> {
    if run_ids.is_empty() {
        return Ok(());
    }
    let ids = serde_json::to_string(run_ids)?;
    let queued = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE id IN (SELECT value FROM json_each(?1)) AND status = 'queued' ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    for run in &queued {
        finish_in_tx(tx, run, "cancelled", Some(ACCESS_LOST_MESSAGE)).await?;
    }
    sqlx::query("UPDATE ci_runs SET cancel_requested = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id IN (SELECT value FROM json_each(?1)) AND status = 'running'")
        .bind(&ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Stops queued and running work in the repositories of these organizations.
pub(crate) async fn stop_organization_ci_runs(
    tx: &mut Transaction<'_, Sqlite>,
    organization_ids: &[i64],
) -> Result<()> {
    if organization_ids.is_empty() {
        return Ok(());
    }
    let organization_ids = serde_json::to_string(organization_ids)?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT c.id FROM ci_runs c JOIN repositories r ON r.id=c.repo_id WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id IN (SELECT value FROM json_each(?1)) AND c.status IN ('queued','running') ORDER BY c.id",
    )
    .bind(organization_ids)
    .fetch_all(&mut **tx)
    .await?;
    stop_ci_runs(tx, &ids).await
}

/// Stops everything `actor` has queued or running, for a ban or a forced
/// password change.
pub(crate) async fn stop_actor_ci_runs(tx: &mut Transaction<'_, Sqlite>, actor: i64) -> Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM ci_runs WHERE triggered_by_id = ?1 AND status IN ('queued', 'running')",
    )
    .bind(actor)
    .fetch_all(&mut **tx)
    .await?;
    stop_ci_runs(tx, &ids).await
}

/// Stops what `actor` has queued or running in one repository, for a removed
/// collaborator.
pub(crate) async fn stop_actor_repo_ci_runs(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo_id: i64,
) -> Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM ci_runs WHERE triggered_by_id = ?1 AND repo_id = ?2 AND status IN ('queued', 'running')",
    )
    .bind(actor)
    .bind(repo_id)
    .fetch_all(&mut **tx)
    .await?;
    stop_ci_runs(tx, &ids).await
}

/// Cancels a queued run immediately, or asks the worker to stop a running one.
pub async fn request_ci_cancellation(
    pool: &SqlitePool,
    run_id: Uuid,
    actor_id: i64,
) -> Result<(bool, Vec<(i64, i64)>)> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let user = sqlx::query_as::<_, super::models::User>("SELECT * FROM users WHERE id = ?1")
        .bind(actor_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(CiAdmissionRefusal::Missing("CI actor unavailable"))?;
    ensure!(
        !user.is_banned && !user.must_change_password,
        CiAdmissionRefusal::Denied("CI actor is restricted")
    );
    let run = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE run_id = ?1")
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(CiAdmissionRefusal::Missing("CI run unavailable"))?;
    let repo =
        sqlx::query_as::<_, super::models::Repository>("SELECT * FROM repositories WHERE id = ?1")
            .bind(run.repo_id)
            .fetch_one(&mut *tx)
            .await?;
    let access =
        super::repositories::access::user_repo_access_in_transaction(&mut tx, actor_id, repo.id)
            .await?;
    let personal_admin = user.is_admin && repo.owner_kind == "user" && repo.owner_org_id.is_none();
    ensure!(
        run.triggered_by_id == Some(actor_id) || personal_admin || access.can_admin(),
        CiAdmissionRefusal::Denied("CI cancellation permission denied")
    );
    let mut notifications = Vec::new();
    match run.status.as_str() {
        "queued" => {
            if let Some((_, created)) = finish_in_tx(
                &mut tx,
                &run,
                "cancelled",
                Some("Cancelled before it started"),
            )
            .await?
            {
                notifications = created;
            }
        }
        "running" => {
            sqlx::query("UPDATE ci_runs SET cancel_requested = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?1")
                .bind(run.id)
                .execute(&mut *tx)
                .await?;
        }
        _ => return Ok((false, notifications)),
    }
    tx.commit().await?;
    Ok((true, notifications))
}

pub async fn get_ci_run_by_run_id(pool: &SqlitePool, run_id: uuid::Uuid) -> Result<Option<CiRun>> {
    let run = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE run_id = ?1")
        .bind(run_id)
        .fetch_optional(pool)
        .await?;

    Ok(run)
}
pub async fn list_ci_runs_for_repo(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
) -> Result<Vec<CiRun>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let rows = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE repo_id = ?1 ORDER BY created_at DESC LIMIT ?2",
    )
    .bind(repo_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}
pub async fn latest_ci_runs_for_commit(
    pool: &SqlitePool,
    repo_id: i64,
    sha: &str,
    pull_request_id: i64,
) -> Result<Vec<CiRun>> {
    Ok(sqlx::query_as::<_, CiRun>(
        "SELECT * FROM (SELECT ci_runs.*, ROW_NUMBER() OVER (
            PARTITION BY instr(workflow_path, ',') > 0,
                CASE WHEN instr(workflow_path, ',') > 0 THEN pipeline ELSE workflow_path END
            ORDER BY id DESC) AS attempt
         FROM ci_runs WHERE repo_id = ?1 AND head_sha = ?2
            AND (pull_request_id IS NULL OR pull_request_id = ?3)) WHERE attempt = 1",
    )
    .bind(repo_id)
    .bind(sha)
    .bind(pull_request_id)
    .fetch_all(pool)
    .await?)
}

/// The status of the latest attempt of each workflow or pipeline on the head commit of each pull
/// request in `pull_request_ids`. It makes the same choice `latest_ci_runs_for_commit` makes for
/// one pull request. A pull request without runs is absent.
pub async fn list_pull_request_check_statuses(
    pool: &SqlitePool,
    pull_request_ids: &[i64],
) -> Result<Vec<(i64, String)>> {
    debug_assert!(pull_request_ids.len() <= 500);
    if pull_request_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<Sqlite>::new(
        "SELECT pull_request_id, status FROM (
            SELECT pulls.id AS pull_request_id, runs.status, ROW_NUMBER() OVER (
                PARTITION BY pulls.id, instr(runs.workflow_path, ',') > 0,
                    CASE WHEN instr(runs.workflow_path, ',') > 0 THEN runs.pipeline ELSE runs.workflow_path END
                ORDER BY runs.id DESC) AS attempt
            FROM pull_requests AS pulls
            JOIN ci_runs AS runs ON runs.repo_id = pulls.repo_id AND runs.head_sha = pulls.head_sha
                AND (runs.pull_request_id IS NULL OR runs.pull_request_id = pulls.id)
            WHERE pulls.id IN (",
    );
    let mut ids = query.separated(",");
    for id in pull_request_ids {
        ids.push_bind(id);
    }
    query.push(")) WHERE attempt = 1");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// The runs `user_id` started, in repositories the user can read today.
///
/// "Can read" is what the repository pages use: the access projection, which
/// covers public repositories, owners, collaborators and organization and team
/// grants, plus site administrators on personal repositories. Hidden runs are
/// dropped before the limit, so they do not use up the page.
pub async fn list_ci_runs_for_user(
    pool: &SqlitePool,
    user_id: i64,
    limit: i64,
) -> Result<Vec<CiRun>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    // Visibility is decided once per repository the user has runs in, in the same
    // statement that orders and limits the runs.
    Ok(sqlx::query_as::<_, CiRun>(&format!(
        "SELECT c.* FROM ci_runs c
         WHERE c.triggered_by_id = ?1 AND c.repo_id IN (
             SELECT r.id FROM repositories r JOIN users u ON u.id = ?1
             WHERE NOT u.is_banned AND NOT u.must_change_password
                 AND r.id IN (SELECT h.repo_id FROM ci_runs h WHERE h.triggered_by_id = ?1)
                 AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) > 0))
         ORDER BY c.created_at DESC, c.id DESC LIMIT ?2"
    ))
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
pub async fn append_ci_logs(
    pool: &SqlitePool,
    run_id: i64,
    worker: &str,
    lines: &[crate::backend::ci::queue::LogLine],
) -> Result<()> {
    let lines = crate::backend::ci::queue::prepare_logs(lines)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let totals: Option<(i64, i64)> = sqlx::query_as(
        "SELECT log_lines, log_bytes FROM ci_runs WHERE id = ?1 AND worker = ?2 AND status = 'running'",
    )
    .bind(run_id)
    .bind(worker)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((stored_lines, stored_bytes)) = totals else {
        bail!("CI run is not owned by this worker");
    };
    let (kept, lines_total, bytes_total) =
        crate::backend::ci::logs::admit_log_lines(stored_lines, stored_bytes, &lines);
    for line in &kept {
        sqlx::query("INSERT INTO ci_logs (run_id, stage, line, level) VALUES (?1, ?2, ?3, ?4)")
            .bind(run_id)
            .bind(&line.stage)
            .bind(&line.line)
            .bind(&line.level)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE ci_runs SET log_lines = ?2, log_bytes = ?3 WHERE id = ?1")
        .bind(run_id)
        .bind(lines_total)
        .bind(bytes_total)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn list_ci_logs(pool: &SqlitePool, run_id: i64, limit: i64) -> Result<Vec<CiLogLine>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let rows = sqlx::query_as::<_, CiLogLine>(
        "SELECT * FROM ci_logs WHERE run_id = ?1 ORDER BY id DESC LIMIT ?2",
    )
    .bind(run_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().rev().collect())
}
pub async fn ci_repository_owner(pool: &SqlitePool, repo_id: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_,Option<String>>(r#"SELECT COALESCE(u.username,o.name) FROM repositories r
        LEFT JOIN users u ON r.owner_kind = 'user' AND r.owner_user_id = u.id
        LEFT JOIN organizations o ON r.owner_kind = 'org' AND r.owner_org_id = o.id WHERE r.id = ?1"#)
        .bind(repo_id).fetch_optional(pool).await?.flatten())
}
/// Transport contract: call only after verifying the accepted ref update. Never
/// expose this as a user assertion or an HTTP endpoint.
pub async fn record_ci_accepted_push(
    pool: &SqlitePool,
    repo_id: i64,
    actor_id: i64,
    reference: &str,
    sha: &str,
) -> Result<()> {
    ensure!(
        (reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/"))
            && sha.len() == 40
            && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid accepted push fact"
    );
    sqlx::query("INSERT INTO ci_accepted_pushes(repo_id,actor_id,reference,sha) VALUES (?1,?2,?3,?4) ON CONFLICT DO NOTHING")
        .bind(repo_id).bind(actor_id).bind(reference).bind(sha.to_ascii_lowercase()).execute(pool).await?;
    Ok(())
}
pub async fn has_ci_accepted_push(
    pool: &SqlitePool,
    repo_id: i64,
    actor_id: i64,
    reference: &str,
    sha: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_accepted_pushes WHERE repo_id = ?1 AND actor_id = ?2 AND reference = ?3 AND sha = ?4)")
        .bind(repo_id).bind(actor_id).bind(reference).bind(sha.to_ascii_lowercase()).fetch_one(pool).await?)
}
pub async fn list_ci_log_history(
    pool: &SqlitePool,
    run_id: i64,
    after: i64,
    limit: i64,
) -> Result<Vec<CiLogLine>> {
    ensure!(
        after >= 0 && (1..=5000).contains(&limit),
        "invalid CI history cursor/window"
    );
    Ok(sqlx::query_as::<_, CiLogLine>(
        "SELECT * FROM ci_logs WHERE run_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
    )
    .bind(run_id)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
pub async fn get_ci_deployment_by_run_id(
    pool: &SqlitePool,
    run_id: i64,
) -> Result<Option<CiDeployment>> {
    let deployment =
        sqlx::query_as::<_, CiDeployment>("SELECT * FROM ci_deployments WHERE run_id = ?1")
            .bind(run_id)
            .fetch_optional(pool)
            .await?;

    Ok(deployment)
}
