use crate::backend::db::action_settings::Refusal;
use crate::backend::db::deployment_reviews::Input;
use crate::backend::db::deployment_reviews::Pending;
use crate::backend::db::deployment_reviews::Review;
use crate::backend::db::deployments::Deployment;
use crate::backend::db::environments::Reviewer;
use anyhow::{ensure, Result};
use codeyard_db_core::models::CiRun;
use sqlx::{PgConnection, PgPool};

async fn can_review(connection: &mut PgConnection, pending: &Pending, actor: i64) -> Result<bool> {
    if pending.environment.prevent_self_review && actor == pending.triggered_by_id {
        return Ok(false);
    }
    let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password)").bind(actor).fetch_one(&mut *connection).await?;
    if !active {
        return Ok(false);
    }
    let reviewers: Vec<Reviewer> = serde_json::from_str(&pending.environment.reviewers)?;
    for reviewer in reviewers {
        if reviewer.kind == "User" && reviewer.id == actor {
            return Ok(true);
        }
        if reviewer.kind == "Team" {
            let member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_team_memberships m JOIN teams t ON t.id=m.team_id JOIN repositories r ON r.owner_org_id=t.org_id AND r.owner_kind='org' WHERE m.team_id=$1 AND m.user_id=$2 AND r.id=$3)").bind(reviewer.id).bind(actor).bind(pending.environment.repo_id).fetch_one(&mut *connection).await?;
            if member {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn pending_rows(connection: &mut PgConnection, repo: i64, run: i64) -> Result<Vec<Pending>> {
    let environment: Option<String> =
        sqlx::query_scalar("SELECT environment FROM ci_deployments WHERE repo_id=$1 AND run_id=$2")
            .bind(repo)
            .bind(run)
            .fetch_optional(&mut *connection)
            .await?;
    let Some(environment) = environment else {
        return Ok(Vec::new());
    };
    Ok(sqlx::query_as("SELECT e.*,d.created_at AS waiting_since,r.triggered_by_id FROM ci_deployments d JOIN ci_runs r ON r.id=d.run_id AND r.repo_id=d.repo_id JOIN deployment_environments e ON e.repo_id=d.repo_id AND e.name_key=$3 WHERE d.repo_id=$1 AND d.run_id=$2 AND d.status='pending' AND (e.wait_timer>0 OR e.reviewers<>'[]') ORDER BY e.id").bind(repo).bind(run).bind(environment.to_lowercase()).fetch_all(connection).await?)
}

pub async fn pending(
    pool: &PgPool,
    repo: i64,
    run: i64,
    actor: Option<i64>,
) -> Result<Vec<Pending>> {
    let mut connection = pool.acquire().await?;
    let mut rows = pending_rows(&mut connection, repo, run).await?;
    if let Some(actor) = actor {
        for row in &mut rows {
            row.can_approve = can_review(&mut connection, row, actor).await?;
        }
    }
    Ok(rows)
}

pub async fn review(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo: i64,
    run: i64,
    input: &Input,
) -> Result<Vec<Deployment>> {
    input.validate()?;
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = pool.begin().await?;
    ensure!(
        super::discussions::lock_read_authority(&mut tx, actor, repo).await?
            && super::credentials::repository(&mut tx, &credential, repo, "deployments", true)
                .await?,
        Refusal::Denied
    );
    let source: CiRun =
        sqlx::query_as("SELECT * FROM ci_runs WHERE repo_id=$1 AND id=$2 FOR UPDATE")
            .bind(repo)
            .bind(run)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    let rows = pending_rows(&mut tx, repo, run).await?;
    let mut deployments = Vec::new();
    for id in &input.environment_ids {
        let pending = rows
            .iter()
            .find(|row| row.environment.id == *id)
            .ok_or(Refusal::Conflict)?;
        // Lock the protection data before the live reviewer check; settings use this same row.
        let env = sqlx::query_as(
            "SELECT * FROM deployment_environments WHERE id=$1 AND repo_id=$2 FOR UPDATE",
        )
        .bind(id)
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
        let current = Pending {
            environment: env,
            waiting_since: pending.waiting_since,
            triggered_by_id: pending.triggered_by_id,
            can_approve: false,
        };
        ensure!(can_review(&mut tx, &current, actor).await?, Refusal::Denied);
        let changed = sqlx::query("UPDATE ci_deployments SET status=$3,approved_by_id=$4,approved_at=NOW(),updated_at=NOW() WHERE repo_id=$1 AND run_id=$2 AND status='pending'").bind(repo).bind(run).bind(&input.state).bind(actor).execute(&mut *tx).await?;
        ensure!(changed.rows_affected() == 1, Refusal::Conflict);
        let sha = source.head_sha.as_deref().ok_or(Refusal::Invalid)?;
        let deployment: Deployment = sqlx::query_as("INSERT INTO repository_deployments(repo_id,creator_id,sha,reference,task,payload,original_environment,environment,production_environment,environment_key) VALUES($1,$2,$3,$4,'deploy',$5,$6,$6,$7,$8) RETURNING *").bind(repo).bind(source.triggered_by_id).bind(sha).bind(&source.branch).bind(serde_json::json!({"workflow_run_id":run}).to_string()).bind(&current.environment.name).bind(current.environment.name_key == "production").bind(&current.environment.name_key).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO deployment_statuses(deployment_id,creator_id,state,environment) VALUES($1,$2,$3,$4)").bind(deployment.id).bind(actor).bind(if input.state == "approved" { "queued" } else { "failure" }).bind(&current.environment.name).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO actions_deployment_reviews(run_id,environment_id,actor_id,state,comment,deployment_id,environment_name) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(run).bind(id).bind(actor).bind(&input.state).bind(&input.comment).bind(deployment.id).bind(&current.environment.name).execute(&mut *tx).await?;
        deployments.push(deployment);
    }
    tx.commit().await?;
    Ok(deployments)
}

pub(super) async fn sync_execution(
    connection: &mut PgConnection,
    run: i64,
    status: &str,
) -> Result<()> {
    let state = crate::backend::db::deployment_reviews::execution_state(status)?;
    sqlx::query("INSERT INTO deployment_statuses(deployment_id,creator_id,state,environment) SELECT DISTINCT d.id,d.creator_id,$2,d.environment FROM actions_deployment_reviews v JOIN repository_deployments d ON d.id=v.deployment_id WHERE v.run_id=$1 AND v.state='approved'")
        .bind(run).bind(state).execute(connection).await?;
    Ok(())
}

pub async fn history(pool: &PgPool, repo: i64, run: i64) -> Result<Vec<Review>> {
    Ok(sqlx::query_as("SELECT v.environment_id,v.environment_name,v.actor_id,v.state,v.comment FROM actions_deployment_reviews v JOIN ci_runs r ON r.id=v.run_id WHERE r.repo_id=$1 AND r.id=$2 ORDER BY v.id").bind(repo).bind(run).fetch_all(pool).await?)
}
