use crate::backend::db::checks::{
    Annotation, Credential, Filter, Preference, Refusal, Run, RunData, RunPatch, Suite,
};
use anyhow::{ensure, Result};
use sqlx::{types::Json, Sqlite, SqlitePool, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Sqlite>,
    credential: Credential<'_>,
    repo: i64,
    admin: bool,
) -> Result<(String, Option<i64>)> {
    let actor: i64 = match credential {
        Credential::Token(id) => sqlx::query_scalar("SELECT t.user_id FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=?1 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND NOT u.is_banned AND NOT u.must_change_password")
            .bind(id).fetch_optional(&mut **tx).await?.ok_or(Refusal::Denied)?,
        Credential::Session(id) => sqlx::query_scalar("SELECT s.user_id FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id=?1 AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT u.is_banned AND NOT u.must_change_password")
            .bind(id).fetch_optional(&mut **tx).await?.ok_or(Refusal::Denied)?,
    };
    ensure!(
        if admin {
            super::discussions::lock_admin_authority(tx, actor, repo).await?
        } else {
            super::discussions::lock_write_authority(tx, actor, repo).await?
        },
        Refusal::Denied
    );
    let mut publisher = format!("user:{actor}");
    let mut app = None;
    if let Credential::Token(id) = credential {
        let live: Option<i64> = sqlx::query_scalar("SELECT id FROM personal_access_tokens WHERE id=?1 AND scope='write' AND (expires_at IS NULL OR expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))")
            .bind(id).fetch_optional(&mut **tx).await?;
        ensure!(live.is_some(), Refusal::Denied);
        let oauth: Option<(String, String, Option<i64>)> = sqlx::query_as("SELECT o.client_id,o.scopes,a.id FROM oauth_app_tokens o LEFT JOIN oauth_applications a ON a.client_id=o.client_id WHERE o.token_id=?1")
            .bind(id).fetch_optional(&mut **tx).await?;
        if let Some((client, scopes, app_id)) = oauth {
            ensure!(
                (app_id.is_some() || client == crate::backend::identity::oauth_apps::CLI_CLIENT)
                    && crate::backend::identity::oauth_apps::allows(
                        &scopes,
                        "/repos/owner/repo/check-runs",
                        true
                    ),
                Refusal::Denied
            );
            if let Some(app_id) = app_id {
                publisher = format!("app:{app_id}");
                app = Some(app_id);
            }
        }
        let bot: bool = sqlx::query_scalar("SELECT username=?2 FROM users WHERE id=?1")
            .bind(actor)
            .bind(crate::backend::db::ci_tokens::BOT)
            .fetch_one(&mut **tx)
            .await?;
        if bot {
            let job: Option<(i64, String, i64, String)> = sqlx::query_as("SELECT t.run_id,t.job,r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=?1 AND t.revoked_at IS NULL AND r.status='running'")
                .bind(id).fetch_optional(&mut **tx).await?;
            let (run, job, job_repo, permissions) = job.ok_or(Refusal::Denied)?;
            let permissions: crate::backend::ci::tokens::Permissions =
                serde_json::from_str(&permissions)?;
            ensure!(
                job_repo == repo && permissions.allows("checks", true) && !admin,
                Refusal::Denied
            );
            publisher = format!("job:{run}:{job}");
        }
    }
    if let Credential::Session(id) = credential {
        let live: Option<i64> = sqlx::query_scalar("SELECT user_id FROM sessions WHERE id=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')").bind(id).fetch_optional(&mut **tx).await?;
        ensure!(live == Some(actor), Refusal::Denied);
    }
    Ok((publisher, app))
}

/// The GitHub App behind the newest check run named `name` in the repository, if that run
/// came from an App.
pub async fn recent_app(pool: &SqlitePool, repo: i64, name: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT g.id FROM check_runs r JOIN check_suites s ON s.id=r.suite_id LEFT JOIN oauth_applications o ON o.id=s.app_id LEFT JOIN github_apps g ON g.client_id=o.client_id WHERE r.repo_id=?1 AND r.name=?2 ORDER BY r.id DESC LIMIT 1")
        .bind(repo)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .flatten())
}

pub async fn application(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<crate::backend::db::oauth_apps::Application>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_applications WHERE id=?1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn pulls(
    pool: &SqlitePool,
    repo: i64,
    sha: &str,
) -> Result<Vec<crate::backend::db::checks::Pull>> {
    Ok(sqlx::query_as("SELECT id,number,head_branch,base_branch,head_sha,base_sha FROM pull_requests WHERE repo_id=?1 AND head_sha=?2 AND state='open' AND head_repo_id=?1 ORDER BY number LIMIT 100").bind(repo).bind(sha).fetch_all(pool).await?)
}
pub async fn suite(pool: &SqlitePool, repo: i64, id: i64) -> Result<Option<Suite>> {
    Ok(
        sqlx::query_as("SELECT * FROM check_suites WHERE repo_id=?1 AND id=?2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn run(pool: &SqlitePool, repo: i64, id: i64) -> Result<Option<Run>> {
    Ok(
        sqlx::query_as("SELECT * FROM check_runs WHERE repo_id=?1 AND id=?2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

async fn insert_suite(
    tx: &mut Transaction<'_, Sqlite>,
    publisher: &str,
    app: Option<i64>,
    repo: i64,
    sha: &str,
    branch: Option<&str>,
) -> Result<(Suite, bool)> {
    let inserted: Option<Suite> = sqlx::query_as("INSERT INTO check_suites(repo_id,publisher,app_id,head_sha,head_branch,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?6) ON CONFLICT(repo_id,publisher,head_sha) DO NOTHING RETURNING *")
        .bind(repo).bind(publisher).bind(app).bind(sha).bind(branch).bind(chrono::Utc::now()).fetch_optional(&mut **tx).await?;
    if let Some(suite) = inserted {
        return Ok((suite, true));
    }
    Ok((
        sqlx::query_as(
            "SELECT * FROM check_suites WHERE repo_id=?1 AND publisher=?2 AND head_sha=?3",
        )
        .bind(repo)
        .bind(publisher)
        .bind(sha)
        .fetch_one(&mut **tx)
        .await?,
        false,
    ))
}
/// The check suite GitHub Actions publishes for a workflow run. Each run attempt has its own,
/// as on GitHub; its publisher names the run, so no credential can write to it.
pub(super) async fn insert_workflow_suite(
    connection: &mut sqlx::SqliteConnection,
    run: &crate::backend::db::models::CiRun,
) -> Result<()> {
    let Some(sha) = run.head_sha.as_deref() else {
        return Ok(());
    };
    let branch = Some(run.head_branch()).filter(|branch| !branch.is_empty());
    sqlx::query("INSERT INTO check_suites(repo_id,publisher,ci_run_id,head_sha,head_branch,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$6)")
        .bind(run.repo_id)
        .bind(format!("actions:{}", run.id))
        .bind(run.id)
        .bind(sha.to_ascii_lowercase())
        .bind(branch)
        .bind(chrono::Utc::now())
        .execute(connection)
        .await?;
    Ok(())
}
pub async fn create_suite(
    pool: &SqlitePool,
    credential: Credential<'_>,
    repo: i64,
    sha: &str,
    branch: Option<&str>,
) -> Result<(Suite, bool)> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (publisher, app) = authority(&mut tx, credential, repo, false).await?;
    let result = insert_suite(&mut tx, &publisher, app, repo, sha, branch).await?;
    tx.commit().await?;
    Ok(result)
}

async fn save(tx: &mut Transaction<'_, Sqlite>, run: &Run, input: &RunPatch) -> Result<()> {
    sqlx::query(
        "UPDATE check_runs SET name=?2,status=?3,conclusion=?4,data=?5,updated_at=?6 WHERE id=?1",
    )
    .bind(run.id)
    .bind(&run.name)
    .bind(&run.status)
    .bind(&run.conclusion)
    .bind(&run.data)
    .bind(chrono::Utc::now())
    .execute(&mut **tx)
    .await?;
    if let Some(output) = &input.output {
        for annotation in &output.annotations {
            sqlx::query("INSERT INTO check_annotations(run_id,data) VALUES(?1,?2)")
                .bind(run.id)
                .bind(Json(annotation))
                .execute(&mut **tx)
                .await?;
        }
    }
    let statuses: Vec<(String, Option<String>)> = sqlx::query_as("SELECT r.status,r.conclusion FROM check_runs r WHERE r.suite_id=?1 AND NOT EXISTS(SELECT 1 FROM check_runs n WHERE n.suite_id=r.suite_id AND n.name=r.name AND n.id>r.id)")
        .bind(run.suite_id).fetch_all(&mut **tx).await?;
    let status = if statuses.iter().any(|(s, _)| s == "in_progress") {
        "in_progress"
    } else if statuses.iter().any(|(s, _)| s != "completed") {
        "queued"
    } else {
        "completed"
    };
    let conclusion = if status == "completed" {
        [
            "failure",
            "timed_out",
            "cancelled",
            "action_required",
            "neutral",
            "skipped",
            "success",
        ]
        .into_iter()
        .find(|c| {
            statuses
                .iter()
                .any(|(_, value)| value.as_deref() == Some(c))
        })
    } else {
        None
    };
    sqlx::query("UPDATE check_suites SET status=?2,conclusion=?3,updated_at=?4 WHERE id=?1")
        .bind(run.suite_id)
        .bind(status)
        .bind(conclusion)
        .bind(chrono::Utc::now())
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub async fn create_run(
    pool: &SqlitePool,
    credential: Credential<'_>,
    repo: i64,
    sha: &str,
    branch: Option<&str>,
    input: &RunPatch,
) -> Result<Run> {
    input.validate(true)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (publisher, app) = authority(&mut tx, credential, repo, false).await?;
    let (suite, _) = insert_suite(&mut tx, &publisher, app, repo, sha, branch).await?;
    let mut run: Run = sqlx::query_as("INSERT INTO check_runs(repo_id,suite_id,name,data,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5) RETURNING *")
        .bind(repo).bind(suite.id).bind(input.name.as_deref().ok_or(Refusal::Invalid)?).bind(Json(RunData::default())).bind(chrono::Utc::now()).fetch_one(&mut *tx).await?;
    input.apply(&mut run)?;
    save(&mut tx, &run, input).await?;
    let result = sqlx::query_as("SELECT * FROM check_runs WHERE id=?1")
        .bind(run.id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn update_run(
    pool: &SqlitePool,
    credential: Credential<'_>,
    repo: i64,
    id: i64,
    input: &RunPatch,
) -> Result<Run> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (publisher, _) = authority(&mut tx, credential, repo, false).await?;
    let suite: Suite = sqlx::query_as("SELECT s.* FROM check_suites s JOIN check_runs r ON r.suite_id=s.id WHERE r.id=?1 AND r.repo_id=?2")
        .bind(id).bind(repo).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    ensure!(suite.publisher == publisher, Refusal::Publisher);
    let mut run: Run = sqlx::query_as("SELECT * FROM check_runs WHERE id=?1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    input.apply(&mut run)?;
    save(&mut tx, &run, input).await?;
    let result = sqlx::query_as("SELECT * FROM check_runs WHERE id=?1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}

const RUNS: &str = " FROM check_runs r JOIN check_suites s ON s.id=r.suite_id WHERE r.repo_id=?1 AND (?2 IS NULL OR r.suite_id=?2) AND (?3 IS NULL OR s.head_sha=?3) AND (?4 IS NULL OR r.name=?4) AND (?5 IS NULL OR r.status=?5) AND (?6 IS NULL OR s.app_id=?6) AND (?7 OR NOT EXISTS(SELECT 1 FROM check_runs n WHERE n.suite_id=r.suite_id AND n.name=r.name AND n.id>r.id))";
pub async fn runs(
    pool: &SqlitePool,
    repo: i64,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<Vec<Run>> {
    Ok(sqlx::query_as(&format!(
        "SELECT r.*{RUNS} ORDER BY r.id DESC LIMIT ?8 OFFSET ?9"
    ))
    .bind(repo)
    .bind(filter.suite_id)
    .bind(&filter.head_sha)
    .bind(&filter.name)
    .bind(&filter.status)
    .bind(filter.app_id)
    .bind(filter.all)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}
pub async fn run_count(pool: &SqlitePool, repo: i64, filter: &Filter) -> Result<i64> {
    Ok(sqlx::query_scalar(&format!("SELECT COUNT(*){RUNS}"))
        .bind(repo)
        .bind(filter.suite_id)
        .bind(&filter.head_sha)
        .bind(&filter.name)
        .bind(&filter.status)
        .bind(filter.app_id)
        .bind(filter.all)
        .fetch_one(pool)
        .await?)
}
const SUITES: &str = " FROM check_suites s WHERE s.repo_id=?1 AND (?2 IS NULL OR s.head_sha=?2) AND (?3 IS NULL OR s.app_id=?3) AND (?4 IS NULL OR EXISTS(SELECT 1 FROM check_runs r WHERE r.suite_id=s.id AND r.name=?4))";
pub async fn suites(
    pool: &SqlitePool,
    repo: i64,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<Vec<Suite>> {
    Ok(sqlx::query_as(&format!(
        "SELECT s.*{SUITES} ORDER BY s.id DESC LIMIT ?5 OFFSET ?6"
    ))
    .bind(repo)
    .bind(&filter.head_sha)
    .bind(filter.app_id)
    .bind(&filter.name)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}
pub async fn suite_count(pool: &SqlitePool, repo: i64, filter: &Filter) -> Result<i64> {
    Ok(sqlx::query_scalar(&format!("SELECT COUNT(*){SUITES}"))
        .bind(repo)
        .bind(&filter.head_sha)
        .bind(filter.app_id)
        .bind(&filter.name)
        .fetch_one(pool)
        .await?)
}
pub async fn annotations(
    pool: &SqlitePool,
    run: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Json<Annotation>>> {
    Ok(sqlx::query_scalar(
        "SELECT data FROM check_annotations WHERE run_id=?1 ORDER BY id LIMIT ?2 OFFSET ?3",
    )
    .bind(run)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}
pub async fn annotation_count(pool: &SqlitePool, run: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM check_annotations WHERE run_id=?1")
            .bind(run)
            .fetch_one(pool)
            .await?,
    )
}
pub async fn rerequest(
    pool: &SqlitePool,
    credential: Credential<'_>,
    repo: i64,
    suite: i64,
    run: Option<i64>,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (publisher, _) = authority(&mut tx, credential, repo, false).await?;
    let stored: Suite = sqlx::query_as("SELECT * FROM check_suites WHERE repo_id=?1 AND id=?2")
        .bind(repo)
        .bind(suite)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    if let Some(run) = run {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM check_runs WHERE id=?1 AND suite_id=?2)",
        )
        .bind(run)
        .bind(suite)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(exists, Refusal::Missing);
        ensure!(stored.publisher == publisher, Refusal::Publisher);
    }
    sqlx::query(
        "UPDATE check_suites SET status='queued',conclusion=NULL,updated_at=?2 WHERE id=?1",
    )
    .bind(suite)
    .bind(chrono::Utc::now())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn preferences(
    pool: &SqlitePool,
    credential: Credential<'_>,
    repo: i64,
    preferences: &[Preference],
) -> Result<Vec<Preference>> {
    ensure!(preferences.len() <= 100, Refusal::Invalid);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, credential, repo, true).await?;
    for preference in preferences {
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_applications WHERE id=?1)")
                .bind(preference.app_id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(known, Refusal::Invalid);
        sqlx::query("INSERT INTO check_suite_preferences(repo_id,app_id,setting) VALUES(?1,?2,?3) ON CONFLICT(repo_id,app_id) DO UPDATE SET setting=excluded.setting")
            .bind(repo).bind(preference.app_id).bind(preference.setting).execute(&mut *tx).await?;
    }
    let result = sqlx::query_as(
        "SELECT app_id,setting FROM check_suite_preferences WHERE repo_id=?1 ORDER BY app_id",
    )
    .bind(repo)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}
