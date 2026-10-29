//! CI runs as a job queue: triggers enqueue, worker nodes claim, heartbeat and
//! finish runs.
use super::models::{CiDeployment, CiLogLine, CiRun};
use crate::backend::db::action_settings::{organization_allows, Setting};
use crate::backend::db::ci::{CiAdmissionRefusal, NewCiRun, ACCESS_LOST_MESSAGE};
use anyhow::{bail, ensure, Result};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

fn is_deployment(run: &CiRun) -> bool {
    matches!(run.pipeline.as_str(), "cd" | "full") || run.environment.is_some()
}

/// Whether `actor` may run (or, with `cd`, deploy) workflows for this repository.
/// Workflows run on the server's own machines, so besides repository access the
/// actor must be a site admin or listed in CI_TRUSTED_ACTOR_IDS.
async fn ci_actor_has_permission(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    repo_id: i64,
    cd: bool,
    trusted_ids: &[i64],
    row_lock: super::repositories::access::RepositoryRowLock,
) -> Result<bool> {
    let Some(authority) = super::repositories::access::lock_user_repo_access_in_transaction(
        tx, actor, repo_id, row_lock, false,
    )
    .await?
    else {
        return Ok(false);
    };
    if !(authority.is_admin || trusted_ids.contains(&actor))
        || (authority.organization && !authority.organization_usable)
    {
        return Ok(false);
    }
    let enabled: bool = sqlx::query_scalar("SELECT COALESCE((SELECT enabled FROM actions_settings WHERE id=1 FOR SHARE),TRUE) AND COALESCE((SELECT enabled FROM repo_actions_settings WHERE repo_id=$1 FOR SHARE),TRUE)")
        .bind(repo_id).fetch_one(&mut **tx).await?;
    if !enabled {
        return Ok(false);
    }
    // The owning organization's Actions permission policy can turn its repositories off.
    let policy: Option<String> = sqlx::query_scalar("SELECT o.rest_settings FROM repositories r JOIN org_actions_settings o ON o.org_id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=$1")
        .bind(repo_id)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some(policy) = policy {
        let policy: serde_json::Value = serde_json::from_str(&policy)?;
        if !organization_allows(&policy, Setting::Permissions, repo_id) {
            return Ok(false);
        }
    }
    let personal_admin = authority.is_admin && !authority.organization;
    Ok(personal_admin
        || if cd {
            authority.access.can_admin()
        } else {
            authority.access.can_write()
        })
}

async fn ci_commit_status(
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
async fn lock_run_repository(tx: &mut Transaction<'_, Postgres>, repo_id: i64) -> Result<()> {
    let _: Option<i64> =
        sqlx::query_scalar("SELECT id FROM repositories WHERE id = $1 FOR KEY SHARE")
            .bind(repo_id)
            .fetch_optional(&mut **tx)
            .await?;
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

pub async fn accept_repository_dispatch(pool: &PgPool, actor: i64, repo: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let allowed = super::discussions::lock_write_authority(&mut tx, actor, repo).await?;
    tx.commit().await?;
    Ok(allowed)
}

pub async fn scheduled_repositories(pool: &PgPool, after: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM repositories WHERE id>$1 AND NOT is_archived ORDER BY id LIMIT 100",
    )
    .bind(after)
    .fetch_all(pool)
    .await?)
}

pub async fn schedule_actor(
    pool: &PgPool,
    repo: i64,
    reference: &str,
    sha: &str,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT COALESCE((SELECT actor_id FROM ci_accepted_pushes WHERE repo_id=$1 AND reference=$2 AND sha=$3 ORDER BY accepted_at DESC LIMIT 1),owner_user_id) FROM repositories WHERE id=$1")
        .bind(repo).bind(reference).bind(sha).fetch_optional(pool).await?.flatten())
}

pub async fn retained_ci_commits(pool: &PgPool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT head_sha FROM ci_runs WHERE repo_id=$1 AND head_sha IS NOT NULL",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

/// Checks permissions and inserts a queued run with its pending commit status
/// (and deployment, for CD pipelines) in one transaction.
pub async fn enqueue_ci_run(
    pool: &PgPool,
    input: NewCiRun<'_>,
    trusted_ids: &[i64],
) -> Result<CiRun> {
    enqueue_ci_run_for_runners(
        pool,
        input,
        trusted_ids,
        &crate::backend::db::runners::Scheduling::default(),
        None,
    )
    .await
}
pub async fn enqueue_ci_run_for_runners(
    pool: &PgPool,
    input: NewCiRun<'_>,
    trusted_ids: &[i64],
    scheduling: &crate::backend::db::runners::Scheduling,
    event_payload: Option<&serde_json::Value>,
) -> Result<CiRun> {
    enqueue_ci_run_inner(pool, input, trusted_ids, None, scheduling, event_payload).await
}
pub async fn enqueue_ci_run_attempt(
    pool: &PgPool,
    input: NewCiRun<'_>,
    trusted_ids: &[i64],
    attempt: crate::backend::db::ci::NewCiAttempt,
    scheduling: &crate::backend::db::runners::Scheduling,
    event_payload: Option<&serde_json::Value>,
) -> Result<CiRun> {
    enqueue_ci_run_inner(
        pool,
        input,
        trusted_ids,
        Some(attempt),
        scheduling,
        event_payload,
    )
    .await
}
async fn enqueue_ci_run_inner(
    pool: &PgPool,
    input: NewCiRun<'_>,
    trusted_ids: &[i64],
    requested_attempt: Option<crate::backend::db::ci::NewCiAttempt>,
    scheduling: &crate::backend::db::runners::Scheduling,
    event_payload: Option<&serde_json::Value>,
) -> Result<CiRun> {
    let mut tx = pool.begin().await?;
    let cd = matches!(input.pipeline, "cd" | "full") || input.environment.is_some();
    let awaiting_approval: bool = !input.manual && input.event == "pull_request" && sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pull_requests WHERE id=$1 AND repo_id=$2 AND head_sha=$3 AND head_repo_id IS DISTINCT FROM repo_id AND workflows_approved_sha IS DISTINCT FROM head_sha AND state='open' AND NOT is_draft)")
        .bind(input.pull_request_id).bind(input.repo_id).bind(input.head_sha).fetch_one(&mut *tx).await?;
    ensure!(
        if awaiting_approval {
            super::discussions::lock_contribution_authority_exclusive(
                &mut tx,
                input.triggered_by_id,
                input.repo_id,
            )
            .await?
        } else {
            ci_actor_has_permission(
                &mut tx,
                input.triggered_by_id,
                input.repo_id,
                cd,
                trusted_ids,
                super::repositories::access::RepositoryRowLock::Exclusive,
            )
            .await?
        },
        CiAdmissionRefusal::Denied("CI repository execution permission denied")
    );
    let paths: Vec<_> = input.workflow_path.split(',').collect();
    let disabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_workflows WHERE repo_id=$1 AND path=ANY($2) AND state<>'active')").bind(input.repo_id).bind(&paths).fetch_one(&mut *tx).await?;
    ensure!(!disabled, CiAdmissionRefusal::WorkflowDisabled);
    if input.event == "workflow_run" && !input.manual {
        if let Some(payload) = event_payload {
            if let Some(run) = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE repo_id=$1 AND workflow_path=$2 AND event='workflow_run' AND event_payload #>> '{workflow_run,id}' = $3 AND event_payload #>> '{workflow_run,run_attempt}' = $4 AND event_payload->>'action'=$5 ORDER BY id LIMIT 1")
                .bind(input.repo_id).bind(input.workflow_path)
                .bind(payload["workflow_run"]["id"].to_string())
                .bind(payload["workflow_run"]["run_attempt"].to_string())
                .bind(payload["action"].as_str()).fetch_optional(&mut *tx).await? {
                return Ok(run);
            }
        }
    }
    let scheduled = (input.event == "schedule" && !input.manual)
        .then(|| event_payload.and_then(|payload| payload["schedule"].as_str()))
        .flatten();
    // Cores that discover the same occurrence share its minute, however late they admit it.
    let minute = scheduled
        .and(event_payload.and_then(|payload| payload["scheduled_minute"].as_i64()))
        .unwrap_or_else(|| chrono::Utc::now().timestamp().div_euclid(60));
    if let Some(cron) = scheduled {
        let queued: Option<Option<i64>> = sqlx::query_scalar("SELECT run_id FROM ci_schedule_runs WHERE repo_id=$1 AND workflow_path=$2 AND cron=$3 AND minute=$4")
            .bind(input.repo_id).bind(input.workflow_path).bind(cron).bind(minute).fetch_optional(&mut *tx).await?;
        if let Some(run_id) = queued {
            // The occurrence stays queued even after its run was deleted.
            let run = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE id=$1")
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await?;
            return run.ok_or_else(|| {
                CiAdmissionRefusal::Conflict("Scheduled run was already queued").into()
            });
        }
    }
    let mut is_fork = false;
    let mut pull_request_id = None;
    let mut pull_request_event = None;
    let mut attempt = 1_i32;
    let mut parent_run = None;
    let mut replay = None;
    if let Some(request) = &requested_attempt {
        if let Some(token) = request.credential {
            let owns:Option<i64>=sqlx::query_scalar("SELECT id FROM personal_access_tokens WHERE id=$1 AND user_id=$2 AND scope='write' AND (expires_at IS NULL OR expires_at>clock_timestamp()) FOR SHARE").bind(token).bind(input.triggered_by_id).fetch_optional(&mut *tx).await?;
            ensure!(
                owns.is_some(),
                CiAdmissionRefusal::Denied("CI credential is no longer authorized")
            );
        }
        let bot: bool = sqlx::query_scalar("SELECT username=$2 FROM users WHERE id=$1")
            .bind(input.triggered_by_id)
            .bind(crate::backend::db::ci_tokens::BOT)
            .fetch_one(&mut *tx)
            .await?;
        if bot {
            let allowed = if let Some(token) = request.credential {
                super::ci_tokens::may_cancel(&mut tx, input.triggered_by_id, input.repo_id, token)
                    .await?
            } else {
                false
            };
            ensure!(
                allowed,
                CiAdmissionRefusal::Denied("CI credential is no longer authorized")
            );
        }
        let source: CiRun =
            sqlx::query_as("SELECT * FROM ci_runs WHERE id=$1 AND repo_id=$2 FOR UPDATE")
                .bind(request.source)
                .bind(input.repo_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(CiAdmissionRefusal::Missing("Run not found"))?;
        crate::backend::db::ci::validate_ci_attempt(&source, &input, request)?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_runs WHERE repo_id=$1 AND COALESCE(workflow_run_id,id)=$2 AND status IN('queued','pending','running','in_progress'))").bind(input.repo_id).bind(source.workflow_run_id.unwrap_or(source.id)).fetch_one(&mut *tx).await?;
        ensure!(
            !active || request.allow_active,
            CiAdmissionRefusal::Conflict("Cannot re-run an active workflow run")
        );
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_run_metadata WHERE run_id=$1)")
                .bind(source.id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            known,
            CiAdmissionRefusal::Conflict("Source workflow metadata is unavailable")
        );
        parent_run = Some(source.id);
        replay = super::action_replays::prepare(&mut tx, &source, request).await?;
    }
    if input.event == "pull_request_target" {
        let id = input.pull_request_id.ok_or(CiAdmissionRefusal::Invalid(
            "CI target run requires a pull request",
        ))?;
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pull_requests WHERE id=$1 AND repo_id=$2 AND base_sha=$3 AND base_branch=$4)")
            .bind(id).bind(input.repo_id).bind(input.head_sha).bind(input.branch).fetch_one(&mut *tx).await?;
        ensure!(
            valid,
            CiAdmissionRefusal::Conflict("CI pull request base changed")
        );
        pull_request_id = Some(id);
    }
    if input.event == "pull_request" {
        let id = match input.pull_request_id {
            Some(id) => id,
            None => {
                let lookup = format!(
                    "SELECT id FROM pull_requests WHERE repo_id = $1 AND head_sha = $2 AND {} AND state = 'open' AND NOT is_draft ORDER BY id LIMIT 2 FOR UPDATE",
                    branch_or_trunk_sql("$3")
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
            "SELECT head_repo_id IS DISTINCT FROM repo_id, COALESCE(workflows_approved_sha = head_sha, FALSE), jsonb_build_object('number', number, 'title', title, 'head_ref', head_branch, 'base_ref', $4::text, 'base_sha', CASE WHEN base_branch = $4 THEN base_sha ELSE (SELECT trunk_layer.base_sha FROM pull_requests trunk_layer WHERE trunk_layer.repo_id = pull_requests.repo_id AND trunk_layer.stack_id = pull_requests.stack_id AND trunk_layer.state = 'open' AND trunk_layer.base_branch = $4 AND NOT EXISTS (SELECT 1 FROM pull_requests below WHERE below.repo_id = trunk_layer.repo_id AND below.stack_id = trunk_layer.stack_id AND below.state = 'open' AND below.head_branch = trunk_layer.base_branch) ORDER BY trunk_layer.id LIMIT 1) END) FROM pull_requests WHERE id = $1 AND repo_id = $2 AND head_sha = $3 AND {} AND ((state = 'open' AND NOT is_draft) OR $5) FOR UPDATE",
            branch_or_trunk_sql("$4")
        );
        let context: Option<(bool, bool, serde_json::Value)> = sqlx::query_as(&context_sql)
            .bind(id)
            .bind(input.repo_id)
            .bind(input.head_sha)
            .bind(input.branch)
            .bind(event_payload.is_some())
            .fetch_optional(&mut *tx)
            .await?;
        let (fork, approved, event) = context.ok_or(CiAdmissionRefusal::Conflict(
            "CI pull request revision is not open",
        ))?;
        is_fork = fork;
        ensure!(
            !fork || super::action_settings::fork_runs_allowed(&mut tx, input.repo_id).await?,
            CiAdmissionRefusal::Denied("Workflows from fork pull requests are turned off for this private repository")
        );
        pull_request_id = Some(id);
        pull_request_event = Some(event);
        ensure!(
            (!fork || approved) && !awaiting_approval || (awaiting_approval && fork && !approved),
            CiAdmissionRefusal::Denied("Fork workflows require approval for this head commit")
        );
        // The newest attempt at this revision is the one automatic admission reuses. A request
        // from a person starts another attempt once that one has finished.
        if let Some(latest) = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE pull_request_id = $1 AND event = 'pull_request' AND branch = $2 AND head_sha = $3 AND workflow_path = $4 ORDER BY attempt DESC LIMIT 1")
            .bind(id).bind(input.branch).bind(input.head_sha).bind(input.workflow_path).fetch_optional(&mut *tx).await? {
            let same_event = input.manual || crate::backend::db::ci::repeats_pull_request_event(latest.event_payload.as_ref(), event_payload);
            if same_event && latest.approval_required && !awaiting_approval && latest.status == "queued" {
                let released: CiRun = sqlx::query_as("UPDATE ci_runs SET approval_required=FALSE,triggered_by_id=$2,updated_at=clock_timestamp() WHERE id=$1 RETURNING *")
                    .bind(latest.id).bind(input.triggered_by_id).fetch_one(&mut *tx).await?;
                schedule_concurrency(&mut tx, &released, scheduling).await?;
                tx.commit().await?;
                return Ok(released);
            }
            if same_event && (!input.manual || matches!(latest.status.as_str(), "queued" | "running")) {
                ensure!(requested_attempt.as_ref().is_none_or(|request|request.allow_active),CiAdmissionRefusal::Conflict("Cannot re-run an active workflow run"));
                return Ok(latest);
            }
            // The attempt keeps this revision's runs unique. A re-run continues the workflow run,
            // while another event at the same revision starts a workflow run of its own.
            attempt = latest.attempt + 1;
            if same_event {
                parent_run = parent_run.or(Some(latest.id));
            }
        }
    }
    let run = sqlx::query_as::<_, CiRun>(
        "INSERT INTO ci_runs (run_id, repo_id, triggered_by_id, mode, pipeline, event, pull_request_id, branch, head_sha, workflow_path, environment, workflow_sha, workflow_blobs, is_fork, pull_request_event, workflow_inputs, attempt, runner_labels, runner_group, event_payload)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$9,$12,$13,$14,$15,$16,$17,$18,$19) RETURNING *",
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
    .bind(input.workflow_blobs)
    .bind(is_fork)
    .bind(pull_request_event)
    .bind(input.workflow_inputs)
    .bind(attempt)
    .bind(serde_json::to_string(&scheduling.labels)?)
    .bind(scheduling.group.as_deref())
    .bind(event_payload)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(cron) = scheduled {
        sqlx::query("INSERT INTO ci_schedule_runs(repo_id,workflow_path,cron,minute,run_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT(repo_id,workflow_path,cron) DO UPDATE SET minute=EXCLUDED.minute,run_id=EXCLUDED.run_id")
            .bind(input.repo_id).bind(input.workflow_path).bind(cron).bind(minute).bind(run.id).execute(&mut *tx).await?;
    }
    let run: CiRun =
        sqlx::query_as("UPDATE ci_runs SET approval_required=$2 WHERE id=$1 RETURNING *")
            .bind(run.id)
            .bind(awaiting_approval)
            .fetch_one(&mut *tx)
            .await?;
    let run = super::action_runs::record(&mut tx, &run, parent_run).await?;
    super::checks::insert_workflow_suite(&mut tx, &run).await?;
    if let Some(request) = &requested_attempt {
        sqlx::query("INSERT INTO actions_replays(run_id,source_id,data,debug) VALUES($1,$2,$3,$4)")
            .bind(run.id)
            .bind(request.source)
            .bind(replay.as_ref().map(sqlx::types::Json))
            .bind(request.debug)
            .execute(&mut *tx)
            .await?;
    }
    if cd {
        sqlx::query("INSERT INTO ci_deployments(run_id, repo_id, environment) VALUES ($1,$2,$3)")
            .bind(run.id)
            .bind(run.repo_id)
            .bind(run.environment.as_deref().unwrap_or("production"))
            .execute(&mut *tx)
            .await?;
    }
    super::deployment_protection::snapshot(&mut tx, &run).await?;
    // Adding retained input changes what a current replica must contain.
    sqlx::query("UPDATE repositories SET write_version=write_version+1 WHERE id=$1 AND storage_node_id IS NOT NULL")
        .bind(run.repo_id).execute(&mut *tx).await?;
    ci_commit_status(&mut tx, &run, "pending", "CI run queued").await?;
    if !awaiting_approval {
        schedule_concurrency(&mut tx, &run, scheduling).await?;
    }
    tx.commit().await?;
    Ok(run)
}

async fn schedule_concurrency(
    tx: &mut Transaction<'_, Postgres>,
    run: &CiRun,
    scheduling: &crate::backend::db::runners::Scheduling,
) -> Result<()> {
    if let Some(config) = &scheduling.concurrency {
        let victims = super::action_concurrency::record(tx, run, config).await?;
        for victim in victims {
            if victim.status == "queued" {
                finish_in_tx(
                    tx,
                    &victim,
                    "cancelled",
                    Some("Superseded in concurrency group"),
                )
                .await?;
            } else {
                sqlx::query("UPDATE ci_runs SET cancel_requested=TRUE WHERE id=$1")
                    .bind(victim.id)
                    .execute(&mut **tx)
                    .await?;
            }
        }
        super::action_concurrency::eligible(tx, run).await?;
    }
    Ok(())
}

pub async fn ci_run_options(
    pool: &PgPool,
    run: i64,
) -> Result<Option<crate::backend::db::ci::CiRunOptions>> {
    Ok(
        sqlx::query_as("SELECT data AS replay,debug FROM actions_replays WHERE run_id=$1")
            .bind(run)
            .fetch_optional(pool)
            .await?,
    )
}

/// Takes the oldest queued run this worker may start, and cancels the queued
/// runs it passes whose starter has lost access since they were queued.
pub async fn claim_ci_run(
    pool: &PgPool,
    worker: &str,
    mode: &str,
    trusted_ids: &[i64],
) -> Result<Option<CiRun>> {
    claim_queued_run(pool, None, worker, mode, trusted_ids).await
}

/// `claim_ci_run` for a cluster node. The node row is locked `FOR SHARE` until the
/// claim commits.
pub async fn claim_ci_run_on_node(
    pool: &PgPool,
    node_key: &str,
    worker: &str,
    mode: &str,
    trusted_ids: &[i64],
) -> Result<Option<CiRun>> {
    claim_queued_run(pool, Some(node_key), worker, mode, trusted_ids).await
}

/// The running run `worker` holds, if an earlier claim committed it and its reply was lost. A
/// worker runs one job at a time, so that run is the one a retried claim should get back. It is
/// locked the same way as a claim: the repository first, then the node, then the run.
async fn held_run(
    tx: &mut Transaction<'_, Postgres>,
    node_key: Option<&str>,
    worker: &str,
) -> Result<Option<CiRun>> {
    let found: Option<(i64, i64)> = sqlx::query_as(
        "SELECT id, repo_id FROM ci_runs WHERE worker = $1 AND status = 'running' ORDER BY id LIMIT 1",
    )
    .bind(worker)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((id, repo_id)) = found else {
        return Ok(None);
    };
    lock_run_repository(tx, repo_id).await?;
    if let Some(node_key) = node_key {
        let active = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM nodes WHERE public_key = $1 AND status = 'active'
             AND 'runner' = ANY(roles) FOR SHARE",
        )
        .bind(node_key)
        .fetch_optional(&mut **tx)
        .await?;
        if active.is_none() {
            return Ok(None);
        }
    }
    let run = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE id = $1 AND worker = $2 AND status = 'running' FOR SHARE",
    )
    .bind(id)
    .bind(worker)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(run)
}

/// Each candidate is checked in its own transaction, so no lock is held while the
/// next one is examined. Within a candidate the locks are taken in the order
/// organization, starter's account, repository, node, then run. Enqueue and the
/// revocations use the same order, so a claim never waits on a ban that is waiting
/// on the claim. A run that another worker or a cancellation holds is left to them.
/// Queued runs are looked at in id order until one can start, so runs that must wait never hide
/// later work.
async fn claim_queued_run(
    pool: &PgPool,
    node_key: Option<&str>,
    worker: &str,
    mode: &str,
    trusted_ids: &[i64],
) -> Result<Option<CiRun>> {
    ensure!(
        matches!(mode, "local" | "podman"),
        "invalid CI execution mode"
    );
    let mut after = 0_i64;
    loop {
        let mut tx = pool.begin().await?;
        // Serialize retries of this worker's claim across cores, before any authority locks.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()), hashtext('ci-worker:' || $1))")
            .bind(worker)
            .execute(&mut *tx)
            .await?;
        let mut held = sqlx::Acquire::begin(&mut tx).await?;
        if let Some(run) = held_run(&mut held, node_key, worker).await? {
            held.commit().await?;
            tx.commit().await?;
            return Ok((!run.cancel_requested).then_some(run));
        }
        // Release authority locks if a previously held run finished while we waited.
        held.rollback().await?;
        let next = sqlx::query_as::<_, CiRun>(
            "SELECT * FROM ci_runs WHERE status = 'queued' AND NOT approval_required AND id > $1 ORDER BY id LIMIT 1",
        )
        .bind(after)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(next) = next else { break };
        after = next.id;
        // A private repository's fork switch may have been turned off after admission.
        let fork_refused = next.is_fork
            && !super::action_settings::fork_runs_allowed(&mut tx, next.repo_id).await?;
        let permitted = match next.triggered_by_id {
            Some(actor) if !next.cancel_requested && !fork_refused => {
                ci_actor_has_permission(
                    &mut tx,
                    actor,
                    next.repo_id,
                    is_deployment(&next),
                    trusted_ids,
                    super::repositories::access::RepositoryRowLock::Shared,
                )
                .await?
            }
            _ => false,
        };
        // The repository comes before the run (see `lock_run_repository`). The check
        // above already holds it unless no check ran or the check stopped early.
        lock_run_repository(&mut tx, next.repo_id).await?;
        if let Some(node_key) = node_key {
            // Held until this claim commits, so a revocation waits for the claim or
            // lands before it.
            let active = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM nodes WHERE public_key = $1 AND status = 'active'
                 AND 'runner' = ANY(roles) FOR SHARE",
            )
            .bind(node_key)
            .fetch_optional(&mut *tx)
            .await?;
            if active.is_none() {
                tx.rollback().await?;
                return Ok(None);
            }
        }
        super::action_concurrency::lock(&mut tx, &next).await?;
        let run = sqlx::query_as::<_, CiRun>(
            "SELECT * FROM ci_runs WHERE id = $1 AND status = 'queued' AND NOT approval_required FOR UPDATE SKIP LOCKED",
        )
        .bind(next.id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(run) = run else {
            tx.rollback().await?;
            continue;
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
            tx.commit().await?;
            continue;
        }
        if let Some(state) = super::deployment_protection::blocked(&mut tx, &run).await? {
            if state == "rejected" {
                finish_in_tx(
                    &mut tx,
                    &run,
                    "failed",
                    Some("Deployment protection rule rejected"),
                )
                .await?;
                tx.commit().await?;
            } else {
                tx.rollback().await?;
            }
            continue;
        }
        if !super::runners::eligible(&mut tx, node_key, &run).await? {
            tx.rollback().await?;
            continue;
        }
        if !super::action_concurrency::eligible(&mut tx, &run).await? {
            tx.rollback().await?;
            continue;
        }
        super::runners::mark_claimed(&mut tx, node_key).await?;
        let run = sqlx::query_as::<_, CiRun>(
            "UPDATE ci_runs SET status = 'running', worker = $2, mode = $3, started_at = clock_timestamp(), heartbeat_at = clock_timestamp(), updated_at = clock_timestamp()
             WHERE id = $1 RETURNING *",
        )
        .bind(run.id)
        .bind(worker)
        .bind(mode)
        .fetch_one(&mut *tx)
        .await?;
        if is_deployment(&run) {
            sqlx::query("UPDATE ci_deployments SET status = 'in_progress', started_at = NOW(), updated_at = NOW() WHERE run_id = $1 AND status = 'pending'")
                .bind(run.id)
                .execute(&mut *tx)
                .await?;
        }
        ci_commit_status(&mut tx, &run, "pending", "CI run in progress").await?;
        tx.commit().await?;
        return Ok(Some(run));
    }
    Ok(None)
}

pub async fn ci_run_allows_repository_read(
    pool: &PgPool,
    run_id: i64,
    repo_id: i64,
    worker: &str,
    public_key: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ci_runs WHERE id = $1 AND repo_id = $2
         AND worker = $3 AND status = 'running' AND NOT cancel_requested
         AND left(worker, length($4) + 1) = $4 || '/')",
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
pub async fn heartbeat_ci_run_on_node(pool: &PgPool, id: i64, worker: &str, node_key: &str) -> Result<Option<bool>> {
    let mut tx = pool.begin().await?;
    let active: Option<i64> = sqlx::query_scalar("SELECT id FROM nodes WHERE public_key=$1 AND status='active' AND 'runner'=ANY(roles) FOR SHARE")
        .bind(node_key).fetch_optional(&mut *tx).await?;
    if active.is_none() || !worker.starts_with(&format!("{node_key}/")) { return Ok(None); }
    let cancelled = sqlx::query_scalar("UPDATE ci_runs SET heartbeat_at=clock_timestamp() WHERE id=$1 AND worker=$2 AND status='running' RETURNING cancel_requested")
        .bind(id).bind(worker).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(cancelled)
}

pub async fn heartbeat_ci_run(pool: &PgPool, id: i64, worker: &str) -> Result<Option<bool>> {
    Ok(sqlx::query_scalar(
        "UPDATE ci_runs SET heartbeat_at = clock_timestamp() WHERE id = $1 AND worker = $2 AND status = 'running' RETURNING cancel_requested",
    )
    .bind(id)
    .bind(worker)
    .fetch_optional(pool)
    .await?)
}

/// Moves a queued or running run to a final state and records the outcome: the
/// run, its deployment, the commit status and, for a failed run, the starter's
/// notification, all in the caller's transaction. The caller holds the run's row lock. A run that is
/// already finished stays as it is and the result is `None`: the first final
/// state wins.
async fn finish_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    run: &CiRun,
    status: &str,
    message: Option<&str>,
) -> Result<Option<(CiRun, Vec<(i64, i64)>)>> {
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
    super::action_jobs::close_active(tx, run.id).await?;
    super::runners::retire(tx, run.worker.as_deref()).await?;
    if is_deployment(run) {
        sqlx::query("UPDATE ci_deployments SET status = $2, completed_at = NOW(), updated_at = NOW() WHERE run_id = $1 AND status IN ('pending','in_progress','approved')")
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
    let description = crate::backend::ci::visible_run_error(message);
    ci_commit_status(
        tx,
        run,
        state,
        description.as_deref().unwrap_or("CI run completed"),
    )
    .await?;
    // Only a failed run tells its starter. Successes and cancellations stay quiet.
    let notifications = if let Some(actor) = run.triggered_by_id.filter(|_| status == "failed") {
        let (title, body) = crate::backend::ci::failure_notification(run, message);
        sqlx::query_as::<_, (i64, i64)>(r#"INSERT INTO notifications(user_id, repo_id, scope_repository_id, kind, title, body, link, metadata)
            SELECT $1, r.id, r.id, 'ci_completed', $2, $3,
                '/' || COALESCE(u.username, o.name) || '/' || r.name || '/actions?job=' || $4,
                jsonb_build_object('run_id', $4::TEXT, 'status', $5::TEXT)
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
        if super::notifications::records::threads::attach_existing(tx, id).await? {
            threaded.push((user, id));
        }
    }
    Ok(Some((finished, threaded)))
}

/// Marks a running run finished, if this worker still owns it. The repository and
/// then the run are locked, so a stale sweep or a cancellation that got there
/// earlier has already decided its outcome and this call changes nothing.
pub async fn finish_ci_run(
    pool: &PgPool,
    id: i64,
    worker: &str,
    status: &str,
    message: Option<&str>,
) -> Result<Option<(CiRun, Vec<(i64, i64)>)>> {
    let mut tx = pool.begin().await?;
    let repo_id: Option<i64> = sqlx::query_scalar(
        "SELECT repo_id FROM ci_runs WHERE id = $1 AND worker = $2 AND status = 'running'",
    )
    .bind(id)
    .bind(worker)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(repo_id) = repo_id else {
        return Ok(None);
    };
    lock_run_repository(&mut tx, repo_id).await?;
    let run = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE id = $1 AND worker = $2 AND status = 'running' FOR UPDATE",
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
///
/// Each run is finished in its own transaction, repository first (see
/// `lock_run_repository`). The staleness test is applied again under the run's
/// lock, so a run whose worker just sent a heartbeat or is finishing it is left
/// alone. Runs another transaction holds are skipped.
pub async fn fail_stale_ci_runs(
    pool: &PgPool,
    stale_seconds: i64,
) -> Result<(Vec<CiRun>, Vec<(i64, i64)>)> {
    let candidates: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT id, repo_id FROM ci_runs WHERE status = 'running' AND (heartbeat_at IS NULL OR heartbeat_at < NOW() - make_interval(secs => $1::double precision))
         ORDER BY id LIMIT 128",
    )
    .bind(stale_seconds)
    .fetch_all(pool)
    .await?;
    let mut finished_runs = Vec::new();
    let mut notifications = Vec::new();
    for (id, repo_id) in candidates {
        let mut tx = pool.begin().await?;
        lock_run_repository(&mut tx, repo_id).await?;
        let run = sqlx::query_as::<_, CiRun>(
            "SELECT * FROM ci_runs WHERE id = $1 AND status = 'running' AND (heartbeat_at IS NULL OR heartbeat_at < NOW() - make_interval(secs => $2::double precision))
             FOR UPDATE SKIP LOCKED",
        )
        .bind(id)
        .bind(stale_seconds)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(run) = run {
            let message = Some("The CI worker stopped before the run finished");
            if let Some((finished, created)) =
                finish_in_tx(&mut tx, &run, "failed", message).await?
            {
                finished_runs.push(finished);
                notifications.extend(created);
            }
        }
        tx.commit().await?;
    }
    Ok((finished_runs, notifications))
}

/// Stops the CI runs in `run_ids` after the people who started them lost access,
/// in the transaction that took the access away. Queued runs are cancelled on the
/// spot, because no worker would ever pick them up again; running ones are asked
/// to stop and the worker cancels them at its next heartbeat. Callers also pass runs
/// that finished within the last hour, the longest a blob grant lives: they keep their
/// result but lose their runtime tokens and blob grants.
pub(crate) async fn stop_ci_runs(
    tx: &mut Transaction<'_, Postgres>,
    run_ids: &[i64],
) -> Result<()> {
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
        .bind(run_ids).fetch_all(&mut **tx).await?;
    let queued = sqlx::query_as::<_, CiRun>(
        "SELECT * FROM ci_runs WHERE id = ANY($1) AND status = 'queued' ORDER BY id FOR UPDATE",
    )
    .bind(run_ids)
    .fetch_all(&mut **tx)
    .await?;
    for run in &queued {
        finish_in_tx(tx, run, "cancelled", Some(ACCESS_LOST_MESSAGE)).await?;
    }
    // Runtime tokens and blob grants end with the access of these runs. Results
    // authentication and downloads check them.
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
pub(crate) async fn stop_organization_ci_runs(
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
pub(crate) async fn stop_actor_ci_runs(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
) -> Result<()> {
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
pub(crate) async fn stop_actor_repo_ci_runs(
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

/// Cancels a queued run immediately, or asks the worker to stop a running one.
/// The run is locked before its status is read, so a worker that is claiming it
/// right now is seen as running and asked to stop, not cancelled underneath.
pub async fn request_ci_cancellation(
    pool: &PgPool,
    run_id: Uuid,
    actor_id: i64,
) -> Result<(bool, Vec<(i64, i64)>)> {
    request_cancellation(pool, run_id, actor_id, None).await
}

pub async fn request_ci_cancellation_with_token(
    pool: &PgPool,
    run_id: Uuid,
    actor_id: i64,
    token_id: i64,
) -> Result<(bool, Vec<(i64, i64)>)> {
    request_cancellation(pool, run_id, actor_id, Some(token_id)).await
}

async fn request_cancellation(
    pool: &PgPool,
    run_id: Uuid,
    actor_id: i64,
    token_id: Option<i64>,
) -> Result<(bool, Vec<(i64, i64)>)> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let user = sqlx::query_as::<_, super::models::User>("SELECT * FROM users WHERE id = $1")
        .bind(actor_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(CiAdmissionRefusal::Missing("CI actor unavailable"))?;
    ensure!(
        !user.is_banned && !user.must_change_password,
        CiAdmissionRefusal::Denied("CI actor is restricted")
    );
    // Authority locks come before the run, including its repository lock.
    let repo_id: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM ci_runs WHERE run_id = $1")
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?;
    let repo_id = repo_id.ok_or(CiAdmissionRefusal::Missing("CI run unavailable"))?;
    let authority = super::repositories::access::lock_user_repo_access_in_transaction(
        &mut tx,
        actor_id,
        repo_id,
        super::repositories::access::RepositoryRowLock::Shared,
        true,
    )
    .await?
    .ok_or(CiAdmissionRefusal::Denied(
        "CI cancellation permission denied",
    ))?;
    let run = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE run_id = $1 FOR UPDATE")
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(CiAdmissionRefusal::Missing("CI run unavailable"))?;
    let personal_admin = authority.is_admin && !authority.organization;
    let workflow_authorized = match token_id {
        Some(token) => super::ci_tokens::may_cancel(&mut tx, actor_id, run.repo_id, token).await?,
        None => false,
    };
    ensure!(
        workflow_authorized
            || crate::backend::db::ci::may_cancel_run(
                actor_id,
                run.triggered_by_id,
                personal_admin,
                authority.access
            ),
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
            sqlx::query(
                "UPDATE ci_runs SET cancel_requested = TRUE, updated_at = NOW() WHERE id = $1",
            )
            .bind(run.id)
            .execute(&mut *tx)
            .await?;
        }
        _ => return Ok((false, notifications)),
    }
    tx.commit().await?;
    Ok((true, notifications))
}

pub async fn delete_ci_run_data(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    id: i64,
    kind: crate::backend::db::ci::CiRunDeletion,
) -> Result<bool> {
    use crate::backend::db::ci::CiRunDeletion;
    let mut tx = pool.begin().await?;
    ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        CiAdmissionRefusal::Denied("CI run deletion permission denied")
    );
    let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM ci_runs WHERE repo_id=$1 AND COALESCE(workflow_run_id,id)=$2 ORDER BY id FOR UPDATE").bind(repo).bind(id).fetch_all(&mut *tx).await?;
    if statuses.is_empty() {
        return Ok(false);
    }
    ensure!(
        statuses
            .iter()
            .all(|status| !matches!(status.as_str(), "queued" | "running")),
        CiAdmissionRefusal::Conflict("Cannot delete an active workflow run or its logs")
    );
    let operation = match kind {
        CiRunDeletion::Run => {
            sqlx::query("DELETE FROM ci_runs WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            "delete"
        }
        CiRunDeletion::Logs => {
            sqlx::query("DELETE FROM ci_logs WHERE run_id IN (SELECT id FROM ci_runs WHERE COALESCE(workflow_run_id,id)=$1)")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE ci_runs SET log_lines=0,log_bytes=0,log_version=log_version+1 WHERE COALESCE(workflow_run_id,id)=$1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            "delete_logs"
        }
    };
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'repository.actions_run','repository',$2,$3)").bind(actor).bind(repo).bind(serde_json::json!({"run_id":id,"operation":operation})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn get_ci_run_by_id(pool: &PgPool, repo_id: i64, id: i64) -> Result<Option<CiRun>> {
    Ok(
        sqlx::query_as("SELECT * FROM ci_runs WHERE repo_id=$1 AND id=$2")
            .bind(repo_id)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// For callers that hold a run's own id but not its repository, such as a worker's report.
pub async fn get_ci_run(pool: &PgPool, id: i64) -> Result<Option<CiRun>> {
    Ok(sqlx::query_as("SELECT * FROM ci_runs WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_ci_run_by_run_id(pool: &PgPool, run_id: uuid::Uuid) -> Result<Option<CiRun>> {
    let run = sqlx::query_as::<_, CiRun>("SELECT * FROM ci_runs WHERE run_id = $1")
        .bind(run_id)
        .fetch_optional(pool)
        .await?;

    Ok(run)
}

pub async fn ci_workflow_paths(pool: &PgPool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT workflow_path FROM ci_runs WHERE repo_id=$1 ORDER BY workflow_path",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_rest_ci_runs(
    pool: &PgPool,
    repo_id: i64,
    filter: &crate::backend::db::ci::RestRunFilter,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<CiRun>)> {
    ensure!(limit >= 0 && offset >= 0, "Invalid run window");
    let condition = "FROM ci_runs c WHERE c.repo_id=$1
        AND ($2::text IS NULL OR c.workflow_path=$2)
        AND ($3::text IS NULL OR CASE WHEN c.event='pull_request' THEN coalesce(c.pull_request_event->>'head_ref',c.branch) ELSE c.branch END IN ($3, 'refs/heads/' || $3))
        AND ($4::text IS NULL OR EXISTS(SELECT 1 FROM users u WHERE u.id=c.triggered_by_id AND lower(u.username)=lower($4)))
        AND ($5::text IS NULL OR c.event=$5)
        AND ($6::text IS NULL OR CASE WHEN $6='completed' THEN c.status NOT IN ('queued','running') WHEN $6='in_progress' THEN c.status='running' WHEN $6='failure' THEN c.status IN ('failed','error') ELSE c.status=$6 END)
        AND ($7::text IS NULL OR c.head_sha=$7)
        AND ($8::timestamptz IS NULL OR c.created_at>=$8)
        AND ($9::timestamptz IS NULL OR c.created_at<=$9)";
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {condition}"))
        .bind(repo_id)
        .bind(&filter.workflow)
        .bind(&filter.branch)
        .bind(&filter.actor)
        .bind(&filter.event)
        .bind(&filter.status)
        .bind(&filter.head_sha)
        .bind(filter.since)
        .bind(filter.until)
        .fetch_one(pool)
        .await?;
    let columns = crate::backend::db::ci::RUN_METADATA_COLUMNS;
    let sql = format!("SELECT {columns} {condition} ORDER BY c.id DESC LIMIT $10 OFFSET $11");
    let rows = sqlx::query_as(&sql)
        .bind(repo_id)
        .bind(&filter.workflow)
        .bind(&filter.branch)
        .bind(&filter.actor)
        .bind(&filter.event)
        .bind(&filter.status)
        .bind(&filter.head_sha)
        .bind(filter.since)
        .bind(filter.until)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok((count, rows))
}

pub async fn list_filtered_ci_runs(
    pool: &PgPool,
    actor: i64,
    repo: Option<i64>,
    filter: &crate::backend::db::ci::CiRunFilter,
    limit: i64,
) -> Result<Vec<CiRun>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    let columns = crate::backend::db::ci::RUN_METADATA_COLUMNS;
    Ok(sqlx::query_as::<_, CiRun>(&format!(
        "WITH history AS (
            SELECT {columns}, r.name AS repository_name,
                CASE WHEN c.event = 'pull_request' THEN coalesce(c.pull_request_event->>'head_ref', c.branch) ELSE c.branch END AS display_ref
            FROM ci_runs c JOIN repositories r ON r.id = c.repo_id
            WHERE (($2::bigint IS NULL AND c.triggered_by_id = $1) OR c.repo_id = $2)
                AND c.repo_id IN (SELECT r.id FROM repositories r JOIN users u ON u.id = $1
                    WHERE NOT u.is_banned AND NOT u.must_change_password
                        AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) > 0))
        ), named AS (SELECT *, CASE WHEN event NOT IN ('pull_request','pull_request_target') AND display_ref LIKE 'refs/heads/%' AND substr(display_ref, 12, 5) != 'refs/' THEN substr(display_ref, 12) ELSE display_ref END AS display_branch FROM history)
        SELECT * FROM named c WHERE ($3 = '' OR EXISTS (SELECT 1 FROM unnest(string_to_array(c.workflow_path, ',')) p WHERE trim(p) = $3))
            AND ($4 = '' OR c.event = $4) AND ($5 = '' OR c.display_branch = $5)
            AND ($6 = '' OR CASE lower(c.status) WHEN 'completed' THEN 'success' WHEN 'error' THEN 'failed' WHEN 'in_progress' THEN 'running' WHEN 'pending' THEN 'queued' WHEN 'canceled' THEN 'cancelled' ELSE lower(c.status) END = $6)
            AND ($7 = '' OR strpos(lower(c.run_id::text || ' ' || c.repository_name || ' ' || c.workflow_path || ' ' || c.display_branch || ' ' || coalesce(c.head_sha, '')), lower($7)) > 0)
            AND ($8::bigint IS NULL OR c.id < $8)
            AND ($10 = '' OR EXISTS (SELECT 1 FROM users actor WHERE actor.id=c.triggered_by_id AND actor.username=$10)) ORDER BY c.id DESC LIMIT $9"
    )).bind(actor).bind(repo).bind(&filter.workflow).bind(&filter.event).bind(&filter.branch).bind(&filter.status).bind(&filter.text).bind(filter.before).bind(limit).bind(&filter.actor).fetch_all(pool).await?)
}

pub async fn list_ci_runs_for_repo(pool: &PgPool, repo_id: i64, limit: i64) -> Result<Vec<CiRun>> {
    let columns = crate::backend::db::ci::RUN_METADATA_COLUMNS;
    let rows = sqlx::query_as::<_, CiRun>(
        &format!("SELECT {columns} FROM ci_runs c WHERE repo_id = $1 ORDER BY created_at DESC LIMIT $2"),
    )
    .bind(repo_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn latest_ci_runs_for_commit(
    pool: &PgPool,
    repo_id: i64,
    sha: &str,
    pull_request_id: i64,
    target_branch: &str,
) -> Result<Vec<CiRun>> {
    Ok(sqlx::query_as::<_, CiRun>(
        "SELECT DISTINCT ON (strpos(workflow_path, ',') > 0,
            CASE WHEN strpos(workflow_path, ',') > 0 THEN pipeline ELSE workflow_path END) *
         FROM ci_runs WHERE repo_id = $1 AND head_sha = $2
            AND (pull_request_id IS NULL OR (pull_request_id = $3 AND branch = $4))
         ORDER BY strpos(workflow_path, ',') > 0,
            CASE WHEN strpos(workflow_path, ',') > 0 THEN pipeline ELSE workflow_path END, id DESC",
    )
    .bind(repo_id)
    .bind(sha)
    .bind(pull_request_id)
    .bind(target_branch)
    .fetch_all(pool)
    .await?)
}

/// The status of the latest attempt of each workflow or pipeline on the head commit of each pull
/// request in `pull_request_ids`. It makes the same choice `latest_ci_runs_for_commit` makes for
/// one pull request. A pull request without runs is absent.
pub async fn list_pull_request_check_statuses(
    pool: &PgPool,
    pull_request_ids: &[i64],
) -> Result<Vec<(i64, String)>> {
    debug_assert!(pull_request_ids.len() <= 500);
    if pull_request_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<Postgres>::new(
        "(SELECT DISTINCT ON (pulls.id, strpos(runs.workflow_path, ',') > 0,
            CASE WHEN strpos(runs.workflow_path, ',') > 0 THEN runs.pipeline ELSE runs.workflow_path END)
            pulls.id AS pull_request_id, runs.status
         FROM pull_requests AS pulls
         JOIN ci_runs AS runs ON runs.repo_id = pulls.repo_id AND runs.head_sha = pulls.head_sha
            AND (runs.pull_request_id IS NULL OR (runs.pull_request_id = pulls.id AND runs.branch = COALESCE(
                (SELECT trunk.base_branch FROM pull_requests trunk
                 WHERE trunk.repo_id = pulls.repo_id AND trunk.stack_id = pulls.stack_id AND trunk.state = 'open'
                   AND NOT EXISTS (SELECT 1 FROM pull_requests below WHERE below.repo_id = trunk.repo_id AND below.stack_id = trunk.stack_id AND below.state = 'open' AND below.head_branch = trunk.base_branch)
                 ORDER BY trunk.id LIMIT 1), pulls.base_branch)))
         WHERE pulls.id IN (",
    );
    let mut ids = query.separated(",");
    for id in pull_request_ids {
        ids.push_bind(id);
    }
    // External commit statuses and the latest check run of each name count too, as on the
    // Checks tab. Statuses under the server's own prefix mirror the runs above.
    query.push(
        ") ORDER BY pulls.id, strpos(runs.workflow_path, ',') > 0,
            CASE WHEN strpos(runs.workflow_path, ',') > 0 THEN runs.pipeline ELSE runs.workflow_path END,
            runs.id DESC)
        UNION ALL SELECT pulls.id, CASE s.state WHEN 'pending' THEN 'queued' ELSE s.state END
            FROM pull_requests pulls JOIN commit_statuses s ON s.repo_id = pulls.repo_id AND s.sha = pulls.head_sha
            WHERE s.ci_run_id IS NULL AND lower(substr(s.context, 1, 8)) <> 'gitserv/' AND pulls.id IN (",
    );
    let mut ids = query.separated(",");
    for id in pull_request_ids {
        ids.push_bind(id);
    }
    query.push(
        ") UNION ALL SELECT pulls.id, CASE WHEN r.status <> 'completed' THEN 'running'
                WHEN r.conclusion IN ('success','neutral','skipped') THEN 'success' ELSE 'failure' END
            FROM pull_requests pulls JOIN check_suites cs ON cs.repo_id = pulls.repo_id AND cs.head_sha = pulls.head_sha
            JOIN check_runs r ON r.suite_id = cs.id
            WHERE NOT EXISTS (SELECT 1 FROM check_runs n WHERE n.suite_id = r.suite_id AND n.name = r.name AND n.id > r.id)
                AND pulls.id IN (",
    );
    let mut ids = query.separated(",");
    for id in pull_request_ids {
        ids.push_bind(id);
    }
    query.push(")");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// The runs `user_id` started, in repositories the user can read today.
///
/// "Can read" is what the repository pages use: the access projection, which
/// covers public repositories, owners, collaborators and organization and team
/// grants, plus site administrators on personal repositories. Hidden runs are
/// dropped before the limit, so they do not use up the page.
pub async fn list_ci_runs_for_user(pool: &PgPool, user_id: i64, limit: i64) -> Result<Vec<CiRun>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    // Visibility is decided once per repository the user has runs in, in the same
    // statement that orders and limits the runs.
    let columns = crate::backend::db::ci::RUN_METADATA_COLUMNS;
    Ok(sqlx::query_as::<_, CiRun>(&format!(
        "SELECT {columns} FROM ci_runs c
         WHERE c.triggered_by_id = $1 AND c.repo_id IN (
             SELECT r.id FROM repositories r JOIN users u ON u.id = $1
             WHERE NOT u.is_banned AND NOT u.must_change_password
                 AND r.id IN (SELECT h.repo_id FROM ci_runs h WHERE h.triggered_by_id = $1)
                 AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) > 0))
         ORDER BY c.created_at DESC, c.id DESC LIMIT $2"
    ))
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn append_ci_logs(
    pool: &PgPool,
    run_id: i64,
    worker: &str,
    lines: &[crate::backend::ci::queue::LogLine],
) -> Result<bool> {
    let lines = crate::backend::ci::queue::prepare_logs(lines)?;
    let mut tx = pool.begin().await?;
    // Serialize log allocation and commit, and check ownership and totals under the same lock.
    let totals: Option<(i64, i64)> = sqlx::query_as(
        "SELECT log_lines, log_bytes FROM ci_runs WHERE id = $1 AND worker = $2 AND status = 'running' FOR UPDATE",
    )
    .bind(run_id)
    .bind(worker)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((stored_lines, stored_bytes)) = totals else {
        bail!("CI run is not owned by this worker");
    };
    let jobs: std::collections::BTreeSet<i64> =
        lines.iter().filter_map(|line| line.job_id).collect();
    if !jobs.is_empty() {
        let ids: Vec<_> = jobs.iter().copied().collect();
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM actions_jobs j JOIN actions_runtime_tokens t ON t.run_id=j.run_id AND t.token_hash=j.generation WHERE j.run_id=$1 AND j.id=ANY($2)").bind(run_id).bind(&ids).fetch_one(&mut *tx).await?;
        ensure!(
            count == ids.len() as i64,
            "log job is outside the active claim"
        );
        let steps: std::collections::BTreeSet<_> = lines
            .iter()
            .filter_map(|line| Some((line.job_id?, line.step_number?)))
            .collect();
        for (job, number) in steps {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM actions_steps WHERE job_id=$1 AND number=$2)",
            )
            .bind(job)
            .bind(number)
            .fetch_one(&mut *tx)
            .await?;
            ensure!(exists, "log step is outside its job");
        }
    }
    let (kept, lines_total, bytes_total) =
        crate::backend::ci::logs::admit_log_lines(stored_lines, stored_bytes, &lines);
    if !kept.is_empty() {
        let mut insert = sqlx::QueryBuilder::<Postgres>::new(
            "INSERT INTO ci_logs (run_id, stage, line, level,job_id,step_number) ",
        );
        insert.push_values(&kept, |mut row, line| {
            row.push_bind(run_id)
                .push_bind(&line.stage)
                .push_bind(&line.line)
                .push_bind(&line.level)
                .push_bind(line.job_id)
                .push_bind(line.step_number);
        });
        insert.build().execute(&mut *tx).await?;
    }
    if lines_total != stored_lines || bytes_total != stored_bytes {
        sqlx::query("UPDATE ci_runs SET log_lines = $2, log_bytes = $3 WHERE id = $1")
            .bind(run_id)
            .bind(lines_total)
            .bind(bytes_total)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(lines_total < crate::backend::ci::logs::MAX_RUN_LOG_LINES)
}

pub async fn list_ci_logs(pool: &PgPool, run_id: i64, limit: i64) -> Result<Vec<CiLogLine>> {
    let rows = sqlx::query_as::<_, CiLogLine>(
        "SELECT * FROM ci_logs WHERE run_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(run_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().rev().collect())
}

pub async fn ci_repository_owner(pool: &PgPool, repo_id: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_,Option<String>>(r#"SELECT COALESCE(u.username,o.name) FROM repositories r
        LEFT JOIN users u ON r.owner_kind = 'user' AND r.owner_user_id = u.id
        LEFT JOIN organizations o ON r.owner_kind = 'org' AND r.owner_org_id = o.id WHERE r.id = $1"#)
        .bind(repo_id).fetch_optional(pool).await?.flatten())
}

/// Transport contract: call only after verifying the accepted ref update. Never
/// expose this as a user assertion or an HTTP endpoint.
pub async fn record_ci_accepted_push(
    pool: &PgPool,
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
    sqlx::query("INSERT INTO ci_accepted_pushes(repo_id,actor_id,reference,sha) VALUES ($1,$2,$3,$4) ON CONFLICT(repo_id,actor_id,reference,sha) DO UPDATE SET accepted_at=excluded.accepted_at")
        .bind(repo_id).bind(actor_id).bind(reference).bind(sha.to_ascii_lowercase()).execute(pool).await?;
    Ok(())
}

pub async fn has_ci_accepted_push(
    pool: &PgPool,
    repo_id: i64,
    actor_id: i64,
    reference: &str,
    sha: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_accepted_pushes WHERE repo_id = $1 AND actor_id = $2 AND reference = $3 AND sha = $4)")
        .bind(repo_id).bind(actor_id).bind(reference).bind(sha.to_ascii_lowercase()).fetch_one(pool).await?)
}

pub async fn list_ci_log_history(
    pool: &PgPool,
    run_id: i64,
    after: i64,
    limit: i64,
) -> Result<Vec<CiLogLine>> {
    ensure!(
        after >= 0 && (1..=5000).contains(&limit),
        "invalid CI history cursor/window"
    );
    Ok(sqlx::query_as::<_, CiLogLine>(
        "SELECT * FROM ci_logs WHERE run_id = $1 AND id > $2 ORDER BY id LIMIT $3",
    )
    .bind(run_id)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn get_ci_deployment_by_run_id(
    pool: &PgPool,
    run_id: i64,
) -> Result<Option<CiDeployment>> {
    let deployment =
        sqlx::query_as::<_, CiDeployment>("SELECT * FROM ci_deployments WHERE run_id = $1")
            .bind(run_id)
            .fetch_optional(pool)
            .await?;

    Ok(deployment)
}
