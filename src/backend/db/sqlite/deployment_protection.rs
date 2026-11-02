use crate::backend::ci::workflows::github_glob_matches;
use crate::backend::db::{
    deployment_protection::{deployment_ref, provider, Gate, Refusal, Review, Rule},
    github_apps::App,
    models::CiRun,
};
use anyhow::Result;
use sqlx::{SqliteConnection, SqlitePool};

pub async fn rules(pool: &SqlitePool, environment: i64) -> Result<Vec<Rule>> {
    Ok(sqlx::query_as(
        "SELECT * FROM deployment_protection_rules WHERE environment_id=?1 AND enabled ORDER BY id",
    )
    .bind(environment)
    .fetch_all(pool)
    .await?)
}

// Keep unavailable Apps out of the chooser, but never silently release their queued gates.
const INTEGRATIONS: &str = "SELECT DISTINCT a.* FROM github_apps a JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN github_app_installations i ON i.app_id=a.id JOIN repositories r ON (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) JOIN github_app_hooks h ON h.app_id=a.id WHERE r.id=?1 AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) AND h.active AND (h.events='*' OR instr(',' || h.events || ',', ',deployment_protection_rule,')>0) ORDER BY a.id";

pub async fn integrations(pool: &SqlitePool, repo: i64) -> Result<Vec<App>> {
    let apps: Vec<App> = sqlx::query_as(INTEGRATIONS)
        .bind(repo)
        .fetch_all(pool)
        .await?;
    Ok(apps.into_iter().filter(provider).collect())
}

pub async fn create(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    environment: i64,
    app: i64,
) -> Result<Rule> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deployment_environments WHERE id=?1 AND repo_id=?2)",
    )
    .bind(environment)
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(exists, Refusal::NotFound);
    let apps: Vec<App> = sqlx::query_as(INTEGRATIONS)
        .bind(repo)
        .fetch_all(&mut *tx)
        .await?;
    let app = apps
        .into_iter()
        .find(|candidate| candidate.id == app && provider(candidate))
        .ok_or(Refusal::Invalid)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deployment_protection_rules WHERE environment_id=?1 AND enabled",
    )
    .bind(environment)
    .fetch_one(&mut *tx)
    .await?;
    let already: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deployment_protection_rules WHERE environment_id=?1 AND app_id=?2 AND enabled)")
        .bind(environment).bind(app.id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(already || count < 6, Refusal::Invalid);
    let rule = sqlx::query_as("INSERT INTO deployment_protection_rules(environment_id,app_id,slug,enabled) VALUES(?1,?2,?3,TRUE) ON CONFLICT(environment_id,app_id) DO UPDATE SET enabled=TRUE,slug=excluded.slug RETURNING *")
        .bind(environment).bind(app.id).bind(app.slug).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(rule)
}

pub async fn disable(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    environment: i64,
    rule: i64,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let changed = sqlx::query("UPDATE deployment_protection_rules SET enabled=FALSE WHERE id=?1 AND environment_id=?2 AND EXISTS(SELECT 1 FROM deployment_environments WHERE id=?2 AND repo_id=?3)")
        .bind(rule).bind(environment).bind(repo).execute(&mut *tx).await?;
    anyhow::ensure!(changed.rows_affected() == 1, Refusal::NotFound);
    sqlx::query("UPDATE deployment_protection_gates SET state='disabled' WHERE rule_id=?1 AND state IN ('pending','rejected')")
        .bind(rule).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn gates(pool: &SqlitePool, run: i64) -> Result<Vec<Gate>> {
    Ok(
        sqlx::query_as(
            "SELECT * FROM deployment_protection_gates WHERE run_id=?1 ORDER BY rule_id",
        )
        .bind(run)
        .fetch_all(pool)
        .await?,
    )
}

/// Snapshot inside admission's repository lock; subsequent rule additions affect only new runs.
pub(super) async fn snapshot(tx: &mut SqliteConnection, run: &CiRun) -> Result<()> {
    let environment = run
        .environment
        .as_deref()
        .or_else(|| matches!(run.pipeline.as_str(), "cd" | "full").then_some("production"));
    let Some(environment) = environment else {
        return Ok(());
    };
    sqlx::query("INSERT INTO deployment_protection_gates(run_id,rule_id,app_id,environment_name,environment_key) SELECT ?1,p.id,p.app_id,e.name,e.name_key FROM deployment_protection_rules p JOIN deployment_environments e ON e.id=p.environment_id WHERE e.repo_id=?2 AND e.name_key=?3 AND p.enabled ON CONFLICT DO NOTHING")
        .bind(run.id).bind(run.repo_id).bind(environment.to_lowercase()).execute(tx).await?;
    Ok(())
}

/// None permits a claim; pending waits and rejected finishes through CI's normal failure path.
/// App protection gates come first, then the environment's own rules.
pub(super) async fn blocked(tx: &mut SqliteConnection, run: &CiRun) -> Result<Option<String>> {
    let gate: Option<String> = sqlx::query_scalar("SELECT state FROM deployment_protection_gates WHERE run_id=?1 AND state IN ('pending','rejected') ORDER BY CASE WHEN state='rejected' THEN 0 ELSE 1 END LIMIT 1")
        .bind(run.id)
        .fetch_optional(&mut *tx)
        .await?;
    if gate.is_some() {
        return Ok(gate);
    }
    native_state(tx, run).await
}

/// Reviewer approval, the wait timer and the branch policy of the run's environment. The review
/// outcome is stored on the run's deployment row.
async fn native_state(tx: &mut SqliteConnection, run: &CiRun) -> Result<Option<String>> {
    let environment: Option<String> =
        sqlx::query_scalar("SELECT environment FROM ci_deployments WHERE run_id=?1")
            .bind(run.id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(environment) = environment else {
        return Ok(None);
    };
    let rules: Option<(i64, String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT e.id, d.status, e.reviewers, e.branch_policy,
                e.wait_timer > 0 AND strftime('%Y-%m-%dT%H:%M:%f000Z', d.created_at, '+' || e.wait_timer || ' minutes') > strftime('%Y-%m-%dT%H:%M:%f000Z', 'now')
           FROM ci_deployments d
           JOIN deployment_environments e ON e.repo_id = d.repo_id AND e.name_key = ?2
          WHERE d.run_id = ?1",
    )
    .bind(run.id)
    .bind(environment.to_lowercase())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((environment_id, status, reviewers, branch_policy, timer_running)) = rules else {
        return Ok(None);
    };
    if status == "rejected" {
        return Ok(Some("rejected".to_owned()));
    }
    let (kind, name) = deployment_ref(&run.branch);
    let branch_allowed = match branch_policy.as_deref() {
        None => true,
        Some("custom") => {
            let patterns: Vec<String> = sqlx::query_scalar(
                "SELECT name FROM deployment_branch_policies WHERE environment_id=?1 AND kind=?2",
            )
            .bind(environment_id)
            .bind(kind)
            .fetch_all(&mut *tx)
            .await?;
            patterns
                .iter()
                .any(|pattern| github_glob_matches(pattern, name))
        }
        // Protected branches: only a branch that matches a branch protection rule may deploy.
        Some(_) if kind == "branch" => {
            let patterns: Vec<String> =
                sqlx::query_scalar("SELECT pattern FROM branch_protection_rules WHERE repo_id=?1")
                    .bind(run.repo_id)
                    .fetch_all(&mut *tx)
                    .await?;
            patterns
                .iter()
                .any(|pattern| github_glob_matches(pattern, name))
        }
        Some(_) => false,
    };
    if !branch_allowed {
        return Ok(Some("rejected".to_owned()));
    }
    if timer_running || (status == "pending" && reviewers != "[]") {
        return Ok(Some("pending".to_owned()));
    }
    Ok(None)
}

pub async fn review(
    pool: &SqlitePool,
    token: i64,
    repo: i64,
    run: i64,
    input: &Review<'_>,
) -> Result<()> {
    input.validate()?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("SELECT id FROM repositories WHERE id=?1")
        .bind(repo)
        .fetch_optional(&mut *tx)
        .await?;
    let credential: Option<(i64, String, String, String)> = sqlx::query_as("SELECT a.id,t.permissions,t.repositories,a.permissions FROM github_app_tokens t JOIN personal_access_tokens p ON p.id=t.token_id JOIN github_app_installations i ON i.id=t.installation_id JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN repositories r ON r.id=?2 WHERE t.token_id=?1 AND NOT t.user_token AND p.user_id=bot.id AND p.scope='write' AND (p.expires_at IS NULL OR p.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id))")
        .bind(token).bind(repo).fetch_optional(&mut *tx).await?;
    let (app, permissions, repositories, granted) = credential.ok_or(Refusal::Denied)?;
    let permissions = serde_json::from_str(&permissions)?;
    let granted = serde_json::from_str(&granted)?;
    let repositories: Vec<i64> = serde_json::from_str(&repositories)?;
    anyhow::ensure!(
        crate::backend::identity::github_apps::allows(&permissions, "deployments", true)
            && crate::backend::identity::github_apps::allows(&granted, "deployments", true)
            && (repositories.is_empty() || repositories.contains(&repo)),
        Refusal::Denied
    );
    let current: Option<(i64, String)> = sqlx::query_as("SELECT id,status FROM ci_runs WHERE repo_id=?1 AND COALESCE(workflow_run_id,id)=?2 ORDER BY COALESCE(workflow_run_attempt,1) DESC,id DESC LIMIT 1")
        .bind(repo).bind(run).fetch_optional(&mut *tx).await?;
    let (id, status) = current.ok_or(Refusal::NotFound)?;
    anyhow::ensure!(status == "queued", Refusal::Conflict);
    let decision: Option<String> = sqlx::query_scalar("SELECT state FROM deployment_protection_gates WHERE run_id=?1 AND app_id=?2 AND environment_key=?3")
        .bind(id).bind(app).bind(input.environment_name.to_lowercase()).fetch_optional(&mut *tx).await?;
    let decision = decision.ok_or(Refusal::NotFound)?;
    anyhow::ensure!(
        decision == "pending" || input.state == Some(decision.as_str()),
        Refusal::Conflict
    );
    sqlx::query("UPDATE deployment_protection_gates SET state=COALESCE(?4,state),comment=COALESCE(?5,comment) WHERE run_id=?1 AND app_id=?2 AND environment_key=?3")
        .bind(id).bind(app).bind(input.environment_name.to_lowercase()).bind(input.state).bind(input.comment).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
