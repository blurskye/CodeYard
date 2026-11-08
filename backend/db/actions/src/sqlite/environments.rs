use crate::environments::validate_branch;
use crate::environments::Branch;
use crate::environments::Environment;
use crate::environments::Refusal;
use crate::environments::Update;
use anyhow::Result;
use codeyard_database::pool::SqlitePool;
use codeyard_database::queries::credentials::Actor;
use sqlx::Sqlite;
use sqlx::Transaction;

async fn write(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
) -> Result<Transaction<'_, Sqlite>> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_database::queries::sqlite::discussions::lock_admin_authority(
            &mut tx,
            actor.user_id,
            repo
        )
        .await?
            && codeyard_database::queries::sqlite::credentials::repository(
                &mut tx,
                &actor,
                repo,
                "administration",
                true
            )
            .await?,
        Refusal::Denied
    );
    Ok(tx)
}

pub async fn get(pool: &SqlitePool, repo: i64, name: &str) -> Result<Option<Environment>> {
    Ok(
        sqlx::query_as("SELECT * FROM deployment_environments WHERE repo_id=?1 AND name_key=?2")
            .bind(repo)
            .bind(name.to_lowercase())
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list(
    pool: &SqlitePool,
    repo: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Environment>> {
    Ok(sqlx::query_as("SELECT * FROM deployment_environments WHERE repo_id=?1 ORDER BY name_key,id LIMIT ?2 OFFSET ?3")
        .bind(repo).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn count(pool: &SqlitePool, repo: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM deployment_environments WHERE repo_id=?1")
            .bind(repo)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn upsert(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    name: &str,
    input: &Update,
) -> Result<Environment> {
    input.validate(name)?;
    let mut tx = write(pool, actor, repo).await?;
    if let Some(reviewers) = &input.reviewers {
        for reviewer in reviewers {
            let valid: bool = if reviewer.kind == "User" {
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT EXISTS(SELECT 1 FROM users u JOIN repositories r ON r.id=?2 WHERE u.id=?1 AND NOT u.is_banned AND NOT u.must_change_password AND (NOT r.is_private OR (r.owner_kind='user' AND (u.is_admin OR r.owner_user_id=u.id)) OR ({})>=1))", codeyard_database::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL)))
                    .bind(reviewer.id).bind(repo).fetch_one(&mut *tx).await?
            } else {
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM teams t JOIN repositories r ON r.owner_org_id=t.org_id AND r.owner_kind='org' WHERE r.id=?2 AND t.id=?1 AND t.privacy IN ('visible','secret') AND (NOT r.is_private OR EXISTS(SELECT 1 FROM team_ancestors inheritance JOIN team_repo_access a ON a.team_id=inheritance.ancestor_id WHERE inheritance.team_id=t.id AND a.repo_id=r.id AND a.permission BETWEEN 1 AND 5)))")
                    .bind(reviewer.id).bind(repo).fetch_one(&mut *tx).await?
            };
            anyhow::ensure!(valid, Refusal::Invalid);
        }
    }
    let reviewers = input
        .reviewers
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let policy = input
        .deployment_branch_policy
        .as_ref()
        .and_then(Option::as_ref)
        .map(|policy| {
            if policy.protected_branches {
                "protected"
            } else {
                "custom"
            }
        });
    let row = sqlx::query_as("INSERT INTO deployment_environments(repo_id,name,name_key,wait_timer,prevent_self_review,reviewers,branch_policy) VALUES(?1,?2,?3,COALESCE(?4,0),COALESCE(?5,false),COALESCE(?6,'[]'),?7) ON CONFLICT(repo_id,name_key) DO UPDATE SET wait_timer=COALESCE(?4,deployment_environments.wait_timer),prevent_self_review=COALESCE(?5,deployment_environments.prevent_self_review),reviewers=COALESCE(?6,deployment_environments.reviewers),branch_policy=CASE WHEN ?8 THEN ?7 ELSE deployment_environments.branch_policy END,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') RETURNING *")
        .bind(repo).bind(name).bind(name.to_lowercase()).bind(input.wait_timer).bind(input.prevent_self_review).bind(reviewers).bind(policy).bind(input.deployment_branch_policy.is_some()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn delete(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    name: &str,
) -> Result<()> {
    let mut tx = write(pool, actor, repo).await?;
    let waiting: Vec<i64> = sqlx::query_scalar("SELECT c.id FROM ci_runs c JOIN ci_deployments d ON d.run_id=c.id WHERE d.repo_id=?1 AND d.environment_key=?2 AND d.status IN ('pending','approved') AND c.status='queued' ORDER BY c.id")
        .bind(repo).bind(name.to_lowercase()).fetch_all(&mut *tx).await?;
    for id in waiting {
        let run = sqlx::query_as("SELECT * FROM ci_runs WHERE id=?1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        codeyard_database::queries::sqlite::ci::finish_in_tx(
            &mut tx,
            &run,
            "failed",
            Some("Deployment environment was deleted"),
        )
        .await?;
    }
    sqlx::query("UPDATE deployment_protection_gates SET state='disabled' WHERE rule_id IN (SELECT p.id FROM deployment_protection_rules p JOIN deployment_environments e ON e.id=p.environment_id WHERE e.repo_id=?1 AND e.name_key=?2) AND state IN ('pending','rejected')")
        .bind(repo).bind(name.to_lowercase()).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM deployment_environments WHERE repo_id=?1 AND name_key=?2")
        .bind(repo)
        .bind(name.to_lowercase())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn branches(
    pool: &SqlitePool,
    environment: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Branch>> {
    Ok(sqlx::query_as("SELECT * FROM deployment_branch_policies WHERE environment_id=?1 ORDER BY id LIMIT ?2 OFFSET ?3").bind(environment).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn branch(pool: &SqlitePool, environment: i64, id: i64) -> Result<Option<Branch>> {
    Ok(
        sqlx::query_as(
            "SELECT * FROM deployment_branch_policies WHERE environment_id=?1 AND id=?2",
        )
        .bind(environment)
        .bind(id)
        .fetch_optional(pool)
        .await?,
    )
}

pub async fn branch_count(pool: &SqlitePool, environment: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM deployment_branch_policies WHERE environment_id=?1",
    )
    .bind(environment)
    .fetch_one(pool)
    .await?)
}

pub async fn save_branch(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    environment: i64,
    id: Option<i64>,
    name: &str,
    kind: &str,
) -> Result<(Branch, bool)> {
    validate_branch(name, kind)?;
    let mut tx = write(pool, actor, repo).await?;
    let enabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deployment_environments WHERE id=?1 AND repo_id=?2 AND branch_policy='custom')").bind(environment).bind(repo).fetch_one(&mut *tx).await?;
    anyhow::ensure!(enabled, Refusal::NotFound);
    let existing: Option<Branch> = sqlx::query_as(
        "SELECT * FROM deployment_branch_policies WHERE environment_id=?1 AND name=?2 AND kind=?3",
    )
    .bind(environment)
    .bind(name)
    .bind(kind)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        if id.is_none() || id == Some(existing.id) {
            tx.commit().await?;
            return Ok((existing, id.is_none()));
        }
        return Err(Refusal::Duplicate.into());
    }
    let row = if let Some(id) = id {
        sqlx::query_as("UPDATE deployment_branch_policies SET name=?3 WHERE environment_id=?1 AND id=?2 RETURNING *").bind(environment).bind(id).bind(name).fetch_optional(&mut *tx).await?.ok_or(Refusal::NotFound)?
    } else {
        sqlx::query_as("INSERT INTO deployment_branch_policies(environment_id,name,kind) VALUES(?1,?2,?3) RETURNING *").bind(environment).bind(name).bind(kind).fetch_one(&mut *tx).await?
    };
    tx.commit().await?;
    Ok((row, false))
}

pub async fn delete_branch(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    environment: i64,
    id: i64,
) -> Result<()> {
    let mut tx = write(pool, actor, repo).await?;
    let result = sqlx::query("DELETE FROM deployment_branch_policies WHERE environment_id=?1 AND id=?2 AND EXISTS(SELECT 1 FROM deployment_environments WHERE id=?1 AND repo_id=?3)").bind(environment).bind(id).bind(repo).execute(&mut *tx).await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::NotFound);
    tx.commit().await?;
    Ok(())
}
