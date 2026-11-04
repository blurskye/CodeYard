use crate::backend::db::checks::{
    Annotation, AutomaticSuites, Credential, Filter, Preference, Publisher, Refusal, Run,
    RunChange, RunData, RunDetails, RunPatch, Suite, SuiteDetails,
};
mod native;
use anyhow::{ensure, Result};
pub use native::page_snapshot;
use sqlx::{types::Json, PgPool, Postgres, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    credential: Credential<'_>,
    repo: i64,
    admin: bool,
) -> Result<(String, Option<i64>)> {
    let actor: i64 = match credential {
        Credential::Token(id) => sqlx::query_scalar("SELECT t.user_id FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND NOT u.is_banned AND NOT u.must_change_password")
            .bind(id).fetch_optional(&mut **tx).await?.ok_or(Refusal::Denied)?,
        Credential::Session(id) => sqlx::query_scalar("SELECT s.user_id FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id=$1 AND s.expires_at>clock_timestamp() AND NOT u.is_banned AND NOT u.must_change_password")
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
        let live: Option<i64> = sqlx::query_scalar("SELECT id FROM personal_access_tokens WHERE id=$1 AND scope='write' AND (expires_at IS NULL OR expires_at>clock_timestamp()) FOR SHARE")
            .bind(id).fetch_optional(&mut **tx).await?;
        ensure!(
            live.is_some()
                && super::fine_grained_tokens::repository_permission(
                    tx, id, actor, repo, "checks", true
                )
                .await?,
            Refusal::Denied
        );
        let oauth: Option<(String, String, Option<i64>, bool)> = sqlx::query_as("SELECT o.client_id,o.scopes,g.id,a.id IS NOT NULL FROM oauth_app_tokens o LEFT JOIN oauth_applications a ON a.client_id=o.client_id LEFT JOIN github_apps g ON g.client_id=o.client_id WHERE o.token_id=$1")
            .bind(id).fetch_optional(&mut **tx).await?;
        if let Some((client, scopes, app_id, registered)) = oauth {
            ensure!(
                (registered || client == codeyard_model::identity::CLI_CLIENT)
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
        // An installation token publishes as its App.
        let installed: Option<i64> = sqlx::query_scalar("SELECT i.app_id FROM github_app_tokens t JOIN github_app_installations i ON i.id=t.installation_id WHERE t.token_id=$1")
            .bind(id).fetch_optional(&mut **tx).await?;
        if let Some(app_id) = installed {
            publisher = format!("app:{app_id}");
            app = Some(app_id);
        }
        let bot: bool = sqlx::query_scalar("SELECT username=$2 FROM users WHERE id=$1")
            .bind(actor)
            .bind(crate::backend::db::ci_tokens::BOT)
            .fetch_one(&mut **tx)
            .await?;
        if bot {
            let job: Option<(i64, String, i64, String)> = sqlx::query_as("SELECT t.run_id,t.job,r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND t.revoked_at IS NULL AND r.status='running' FOR SHARE OF t,r")
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
        let live: Option<i64> = sqlx::query_scalar(
            "SELECT user_id FROM sessions WHERE id=$1 AND expires_at>clock_timestamp() FOR SHARE",
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
        ensure!(live == Some(actor), Refusal::Denied);
    }
    Ok((publisher, app))
}

/// The most recent App producer of this name. A newer account-owned run must not erase its pin.
pub async fn recent_app(pool: &PgPool, repo: i64, name: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT s.app_id FROM check_runs r JOIN check_suites s ON s.id=r.suite_id WHERE r.repo_id=$1 AND r.name=$2 AND s.app_id IS NOT NULL ORDER BY r.id DESC LIMIT 1")
        .bind(repo)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .flatten())
}

pub async fn publishers(pool: &PgPool, ids: &[i64]) -> Result<Vec<Publisher>> {
    crate::backend::db::checks::projection_ids(ids)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as("SELECT a.id,a.client_id,a.name,a.homepage,a.created_at,a.owner_id,u.username AS owner_login,u.is_admin AS owner_is_admin FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.id=ANY($1)").bind(ids).fetch_all(pool).await?)
}

pub async fn run_details(pool: &PgPool, repo: i64, ids: &[i64]) -> Result<Vec<RunDetails>> {
    crate::backend::db::checks::projection_ids(ids)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as("SELECT r.id AS run_id,s.*,(SELECT COUNT(*) FROM check_annotations a WHERE a.run_id=r.id) AS annotation_count FROM check_runs r JOIN check_suites s ON s.id=r.suite_id AND s.repo_id=r.repo_id WHERE r.repo_id=$1 AND r.id=ANY($2)").bind(repo).bind(ids).fetch_all(pool).await?)
}

pub async fn suite_details(pool: &PgPool, repo: i64, ids: &[i64]) -> Result<Vec<SuiteDetails>> {
    crate::backend::db::checks::projection_ids(ids)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // A run held by an approval or a deployment protection rule shows as waiting, as its run does.
    let waiting = crate::backend::db::deployment_protection::waiting_sql(true, "c");
    Ok(sqlx::query_as(&format!("SELECT s.id AS suite_id,(SELECT COUNT(DISTINCT r.name) FROM check_runs r WHERE r.suite_id=s.id AND r.repo_id=s.repo_id) AS run_count,c.status AS ci_status,(COALESCE(c.approval_required,FALSE) OR COALESCE({waiting},FALSE)) AS ci_approval_required FROM check_suites s LEFT JOIN ci_runs c ON c.id=s.ci_run_id AND c.repo_id=s.repo_id WHERE s.repo_id=$1 AND s.id=ANY($2)")).bind(repo).bind(ids).fetch_all(pool).await?)
}

pub async fn pulls_many(
    pool: &PgPool,
    repo: i64,
    heads: &[String],
) -> Result<Vec<crate::backend::db::checks::Pull>> {
    crate::backend::db::checks::projection_heads(heads)?;
    if heads.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as("SELECT p.* FROM (SELECT DISTINCT unnest($2::text[]) AS sha) h CROSS JOIN LATERAL (SELECT id,number::BIGINT AS number,head_branch,base_branch,head_sha,base_sha FROM pull_requests WHERE repo_id=$1 AND head_sha=h.sha AND state='open' AND head_repo_id=repo_id ORDER BY number LIMIT 100) p ORDER BY p.head_sha,p.number").bind(repo).bind(heads).fetch_all(pool).await?)
}

pub async fn pulls(
    pool: &PgPool,
    repo: i64,
    sha: &str,
) -> Result<Vec<crate::backend::db::checks::Pull>> {
    pulls_many(pool, repo, &[sha.to_owned()]).await
}
pub async fn suite(pool: &PgPool, repo: i64, id: i64) -> Result<Option<Suite>> {
    Ok(
        sqlx::query_as("SELECT * FROM check_suites WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn run(pool: &PgPool, repo: i64, id: i64) -> Result<Option<Run>> {
    Ok(
        sqlx::query_as("SELECT * FROM check_runs WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

async fn insert_suite(
    tx: &mut Transaction<'_, Postgres>,
    publisher: &str,
    app: Option<i64>,
    repo: i64,
    sha: &str,
    branch: Option<&str>,
) -> Result<(Suite, bool)> {
    let inserted: Option<Suite> = sqlx::query_as("INSERT INTO check_suites(repo_id,publisher,app_id,head_sha,head_branch,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$6) ON CONFLICT(repo_id,publisher,head_sha) DO NOTHING RETURNING *")
        .bind(repo).bind(publisher).bind(app).bind(sha).bind(branch).bind(chrono::Utc::now()).fetch_optional(&mut **tx).await?;
    if let Some(suite) = inserted {
        return Ok((suite, true));
    }
    Ok((sqlx::query_as("SELECT * FROM check_suites WHERE repo_id=$1 AND publisher=$2 AND head_sha=$3 FOR UPDATE")
        .bind(repo).bind(publisher).bind(sha).fetch_one(&mut **tx).await?, false))
}
async fn lock_preferences(tx: &mut Transaction<'_, Postgres>, repo: i64) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(repo ^ 0x434B_0000_0000_0000_i64)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn automatic_suites(
    pool: &PgPool,
    repo: &crate::backend::db::models::Repository,
    sha: &str,
    branch: &str,
    after: i64,
) -> Result<AutomaticSuites> {
    crate::backend::db::checks::validate_automatic(sha, branch, after)?;
    let mut page = AutomaticSuites {
        after,
        ..Default::default()
    };
    let mut tx = pool.begin().await?;
    let current: Option<crate::backend::db::models::Repository> = sqlx::query_as(
        "SELECT * FROM repositories WHERE id=$1 AND NOT is_archived AND NOT is_gist FOR SHARE",
    )
    .bind(repo.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Ok(page);
    };
    if current.owner_kind != repo.owner_kind
        || current.owner_user_id != repo.owner_user_id
        || current.owner_org_id != repo.owner_org_id
    {
        return Ok(page);
    }
    lock_preferences(&mut tx, repo.id).await?;
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    let mut apps: Vec<i64> = sqlx::query_scalar(&format!("SELECT a.id FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN oauth_applications o ON o.client_id=a.client_id JOIN users owner ON owner.id=a.owner_id JOIN users u ON u.id=a.bot_id JOIN repositories r ON r.id=$1 LEFT JOIN check_suite_preferences p ON p.repo_id=r.id AND p.app_id=a.id WHERE a.id>$2 AND COALESCE(p.setting,TRUE) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT u.is_banned AND NOT u.must_change_password AND (a.permissions::jsonb->>'checks') IN ('write','admin') AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) AND ({access})>=3 ORDER BY a.id LIMIT 101 FOR SHARE OF i,a,o,owner,u"))
        .bind(repo.id).bind(after).fetch_all(&mut *tx).await?;
    page.has_next = apps.len() > 100;
    apps.truncate(100);
    page.after = apps.last().copied().unwrap_or(after);
    let sha = sha.to_ascii_lowercase();
    for app in apps {
        let (suite, created) = insert_suite(
            &mut tx,
            &format!("app:{app}"),
            Some(app),
            repo.id,
            &sha,
            Some(branch),
        )
        .await?;
        if created {
            page.suites.push(suite);
        }
    }
    tx.commit().await?;
    Ok(page)
}

/// The check suite GitHub Actions publishes for a workflow run. Each run attempt has its own,
/// as on GitHub; its publisher names the run, so no credential can write to it.
pub(super) async fn insert_workflow_suite(
    connection: &mut sqlx::PgConnection,
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
    pool: &PgPool,
    credential: Credential<'_>,
    repo: i64,
    sha: &str,
    branch: Option<&str>,
) -> Result<(Suite, bool)> {
    let mut tx = pool.begin().await?;
    let (publisher, app) = authority(&mut tx, credential, repo, false).await?;
    let result = insert_suite(&mut tx, &publisher, app, repo, sha, branch).await?;
    tx.commit().await?;
    Ok(result)
}

async fn save(tx: &mut Transaction<'_, Postgres>, run: &Run, input: &RunPatch) -> Result<Suite> {
    sqlx::query(
        "UPDATE check_runs SET name=$2,status=$3,conclusion=$4,data=$5,updated_at=$6 WHERE id=$1",
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
            sqlx::query("INSERT INTO check_annotations(run_id,data) VALUES($1,$2)")
                .bind(run.id)
                .bind(Json(annotation))
                .execute(&mut **tx)
                .await?;
        }
    }
    let statuses: Vec<(String, Option<String>)> = sqlx::query_as("SELECT r.status,r.conclusion FROM check_runs r WHERE r.suite_id=$1 AND NOT EXISTS(SELECT 1 FROM check_runs n WHERE n.suite_id=r.suite_id AND n.name=r.name AND n.id>r.id)")
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
    Ok(sqlx::query_as(
        "UPDATE check_suites SET status=$2,conclusion=$3,updated_at=$4 WHERE id=$1 RETURNING *",
    )
    .bind(run.suite_id)
    .bind(status)
    .bind(conclusion)
    .bind(chrono::Utc::now())
    .fetch_one(&mut **tx)
    .await?)
}
pub(crate) async fn allocate_run_id(connection: &mut sqlx::PgConnection) -> Result<i64> {
    Ok(
        sqlx::query_scalar("INSERT INTO check_run_identities DEFAULT VALUES RETURNING id")
            .fetch_one(connection)
            .await?,
    )
}

pub async fn create_run(
    pool: &PgPool,
    credential: Credential<'_>,
    repo: i64,
    sha: &str,
    branch: Option<&str>,
    input: &RunPatch,
) -> Result<RunChange> {
    input.validate(true)?;
    let mut tx = pool.begin().await?;
    let (publisher, app) = authority(&mut tx, credential, repo, false).await?;
    let (suite, _) = insert_suite(&mut tx, &publisher, app, repo, sha, branch).await?;
    let id = allocate_run_id(&mut tx).await?;
    let mut run: Run = sqlx::query_as("INSERT INTO check_runs(repo_id,suite_id,name,data,created_at,updated_at,id) VALUES($1,$2,$3,$4,$5,$5,$6) RETURNING *")
        .bind(repo).bind(suite.id).bind(input.name.as_deref().ok_or(Refusal::Invalid)?).bind(Json(RunData::default())).bind(chrono::Utc::now()).bind(id).fetch_one(&mut *tx).await?;
    input.apply(&mut run)?;
    let saved_suite = save(&mut tx, &run, input).await?;
    let completed_suite =
        (suite.status != "completed" && saved_suite.status == "completed").then_some(saved_suite);
    let result = sqlx::query_as("SELECT * FROM check_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(RunChange {
        run: result,
        completed_suite,
    })
}
pub async fn update_run(
    pool: &PgPool,
    credential: Credential<'_>,
    repo: i64,
    id: i64,
    input: &RunPatch,
) -> Result<RunChange> {
    let mut tx = pool.begin().await?;
    let (publisher, _) = authority(&mut tx, credential, repo, false).await?;
    let suite: Suite = sqlx::query_as("SELECT s.* FROM check_suites s JOIN check_runs r ON r.suite_id=s.id WHERE r.id=$1 AND r.repo_id=$2 FOR UPDATE OF s")
        .bind(id).bind(repo).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    ensure!(suite.publisher == publisher, Refusal::Publisher);
    let mut run: Run = sqlx::query_as("SELECT * FROM check_runs WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    input.apply(&mut run)?;
    let saved_suite = save(&mut tx, &run, input).await?;
    let completed_suite =
        (suite.status != "completed" && saved_suite.status == "completed").then_some(saved_suite);
    let result = sqlx::query_as("SELECT * FROM check_runs WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(RunChange {
        run: result,
        completed_suite,
    })
}

const RUNS: &str = " FROM check_runs r JOIN check_suites s ON s.id=r.suite_id WHERE r.repo_id=$1 AND ($2::bigint IS NULL OR r.suite_id=$2) AND ($3::text IS NULL OR s.head_sha=$3) AND ($4::text IS NULL OR r.name=$4) AND ($5::text IS NULL OR r.status=$5) AND ($6::bigint IS NULL OR s.app_id=$6) AND ($7 OR NOT EXISTS(SELECT 1 FROM check_runs n WHERE n.suite_id=r.suite_id AND n.name=r.name AND n.id>r.id))";
pub async fn rollup_runs(
    pool: &PgPool,
    repo: i64,
    sha: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<Run>> {
    anyhow::ensure!(offset >= 0 && (0..=100).contains(&limit), Refusal::Invalid);
    Ok(sqlx::query_as(&format!("SELECT r.id,r.repo_id,r.suite_id,r.name,r.status,r.conclusion,jsonb_build_object('details_url',r.data->'details_url','external_id',r.data->'external_id','started_at',r.data->'started_at','completed_at',r.data->'completed_at','title',r.data->'title','images','[]'::jsonb,'actions','[]'::jsonb) AS data,r.created_at,r.updated_at{RUNS} ORDER BY r.id DESC LIMIT $8 OFFSET $9"))
        .bind(repo).bind(None::<i64>).bind(sha).bind(None::<String>).bind(None::<String>).bind(None::<i64>).bind(false).bind(limit).bind(offset).fetch_all(pool).await?)
}
pub async fn rollup_counts(pool: &PgPool, repo: i64, sha: &str) -> Result<Vec<(String, i64)>> {
    Ok(sqlx::query_as(&format!("SELECT UPPER(CASE WHEN r.status='completed' THEN COALESCE(r.conclusion,r.status) ELSE r.status END),COUNT(*){RUNS} GROUP BY 1 ORDER BY 1"))
        .bind(repo).bind(None::<i64>).bind(sha).bind(None::<String>).bind(None::<String>).bind(None::<i64>).bind(false).fetch_all(pool).await?)
}
pub async fn workflow_metadata(
    pool: &PgPool,
    repo: i64,
    suite: i64,
) -> Result<Option<(String, String, Option<String>)>> {
    Ok(sqlx::query_as("SELECT c.event,c.workflow_path,w.name FROM check_suites s JOIN ci_runs c ON c.id=s.ci_run_id AND c.repo_id=s.repo_id LEFT JOIN actions_workflows w ON w.repo_id=c.repo_id AND w.path=c.workflow_path WHERE s.repo_id=$1 AND s.id=$2").bind(repo).bind(suite).fetch_optional(pool).await?)
}

pub async fn runs(
    pool: &PgPool,
    repo: i64,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<Vec<Run>> {
    Ok(sqlx::query_as(&format!(
        "SELECT r.*{RUNS} ORDER BY r.id DESC LIMIT $8 OFFSET $9"
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
pub async fn summary(
    pool: &PgPool,
    repo: i64,
    sha: &str,
) -> Result<crate::backend::db::checks::CheckSummary> {
    Ok(sqlx::query_as(&format!("SELECT COUNT(*) AS total, COALESCE(SUM(CASE WHEN r.status='completed' AND (r.conclusion IS NULL OR r.conclusion NOT IN ('success','neutral','skipped')) THEN 1 ELSE 0 END),0)::BIGINT AS failed, COALESCE(SUM(CASE WHEN r.status<>'completed' THEN 1 ELSE 0 END),0)::BIGINT AS pending{RUNS}"))
        .bind(repo).bind(None::<i64>).bind(sha).bind(None::<&str>).bind(None::<&str>)
        .bind(None::<i64>).bind(false).fetch_one(pool).await?)
}

pub async fn run_count(pool: &PgPool, repo: i64, filter: &Filter) -> Result<i64> {
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
const SUITES: &str = " FROM check_suites s WHERE s.repo_id=$1 AND ($2::text IS NULL OR s.head_sha=$2) AND ($3::bigint IS NULL OR CASE WHEN s.ci_run_id IS NOT NULL THEN 15368 ELSE s.app_id END=$3) AND ($4::text IS NULL OR EXISTS(SELECT 1 FROM check_runs r WHERE r.suite_id=s.id AND r.name=$4))";
pub async fn suites(
    pool: &PgPool,
    repo: i64,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<Vec<Suite>> {
    Ok(sqlx::query_as(&format!(
        "SELECT s.*{SUITES} ORDER BY s.id DESC LIMIT $5 OFFSET $6"
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
pub async fn suite_count(pool: &PgPool, repo: i64, filter: &Filter) -> Result<i64> {
    Ok(sqlx::query_scalar(&format!("SELECT COUNT(*){SUITES}"))
        .bind(repo)
        .bind(&filter.head_sha)
        .bind(filter.app_id)
        .bind(&filter.name)
        .fetch_one(pool)
        .await?)
}
pub async fn annotations(
    pool: &PgPool,
    run: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Json<Annotation>>> {
    Ok(sqlx::query_scalar(
        "SELECT data FROM check_annotations WHERE run_id=$1 ORDER BY id LIMIT $2 OFFSET $3",
    )
    .bind(run)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}
pub async fn annotation_count(pool: &PgPool, run: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM check_annotations WHERE run_id=$1")
            .bind(run)
            .fetch_one(pool)
            .await?,
    )
}
pub async fn requested_action(
    pool: &PgPool,
    credential: Credential<'_>,
    repo: i64,
    run: i64,
    identifier: &str,
) -> Result<Run> {
    let mut tx = pool.begin().await?;
    authority(&mut tx, credential, repo, false).await?;
    let run: Run = sqlx::query_as("SELECT r.* FROM check_runs r JOIN check_suites s ON s.id=r.suite_id WHERE r.id=$1 AND r.repo_id=$2 AND s.ci_run_id IS NULL FOR UPDATE OF s,r")
        .bind(run).bind(repo).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    ensure!(
        run.data
            .actions
            .iter()
            .any(|action| action.identifier == identifier),
        Refusal::Invalid
    );
    tx.commit().await?;
    Ok(run)
}

pub async fn rerequest(
    pool: &PgPool,
    credential: Credential<'_>,
    repo: i64,
    suite: i64,
    run: Option<i64>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let (publisher, _) = authority(&mut tx, credential, repo, false).await?;
    let stored: Suite =
        sqlx::query_as("SELECT * FROM check_suites WHERE repo_id=$1 AND id=$2 FOR UPDATE")
            .bind(repo)
            .bind(suite)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    if let Some(run) = run {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM check_runs WHERE id=$1 AND suite_id=$2)",
        )
        .bind(run)
        .bind(suite)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(exists, Refusal::Missing);
        ensure!(stored.publisher == publisher, Refusal::Publisher);
    }
    sqlx::query(
        "UPDATE check_suites SET status='queued',conclusion=NULL,updated_at=$2 WHERE id=$1",
    )
    .bind(suite)
    .bind(chrono::Utc::now())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn preferences(
    pool: &PgPool,
    credential: Credential<'_>,
    repo: i64,
    preferences: &[Preference],
) -> Result<Vec<Preference>> {
    ensure!(preferences.len() <= 100, Refusal::Invalid);
    let mut tx = pool.begin().await?;
    authority(&mut tx, credential, repo, true).await?;
    lock_preferences(&mut tx, repo).await?;
    for preference in preferences {
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_apps WHERE id=$1)")
                .bind(preference.app_id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(known, Refusal::Invalid);
        sqlx::query("INSERT INTO check_suite_preferences(repo_id,app_id,setting) VALUES($1,$2,$3) ON CONFLICT(repo_id,app_id) DO UPDATE SET setting=excluded.setting")
            .bind(repo).bind(preference.app_id).bind(preference.setting).execute(&mut *tx).await?;
    }
    let result = sqlx::query_as(
        "SELECT app_id,setting FROM check_suite_preferences WHERE repo_id=$1 ORDER BY app_id",
    )
    .bind(repo)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}
