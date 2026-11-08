use crate::code_scanning::changes;
use crate::code_scanning::instance_condition;
use crate::code_scanning::Alert;
use crate::code_scanning::AlertFilters;
use crate::code_scanning::AlertScope;
use crate::code_scanning::Analysis;
use crate::code_scanning::Change;
use crate::code_scanning::Dismissal;
use crate::code_scanning::Finding;
use crate::code_scanning::Instance;
use crate::code_scanning::NewUpload;
use crate::code_scanning::OrganizationAlert;
use crate::code_scanning::Refusal;
use crate::code_scanning::Run;
use crate::code_scanning::Upload;
use crate::code_scanning::ANALYSIS_COLUMNS;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::pool::SqlitePool;
use sqlx::SqliteConnection;
pub(crate) mod list;
pub(crate) mod pulls;
pub(crate) mod read;

pub async fn enqueue(pool: &SqlitePool, input: &NewUpload<'_>) -> Result<Upload> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_write_authority(
            &mut tx,
            input.actor.user_id,
            input.repo
        )
        .await?
            && codeyard_db_core::queries::sqlite::credentials::repository(
                &mut tx,
                &input.actor,
                input.repo,
                "security_events",
                true
            )
            .await?,
        Refusal::Denied
    );
    let recent: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_scanning_uploads WHERE actor_id=$1 AND created_at>=$2 AND (repo_id=$3 OR NOT EXISTS(SELECT 1 FROM users WHERE id=$1 AND username=$4))")
        .bind(input.actor.user_id).bind(chrono::Utc::now()-chrono::Duration::hours(1)).bind(input.repo).bind(codeyard_db_core::queries::ci_tokens::BOT).fetch_one(&mut *tx).await?;
    ensure!(recent < 1000, Refusal::RateLimited);
    let id = uuid::Uuid::new_v4().to_string();
    let upload = sqlx::query_as("INSERT INTO code_scanning_uploads(id,repo_id,actor_id,ref_name,commit_sha,sarif,checkout_uri,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING *")
        .bind(id).bind(input.repo).bind(input.actor.user_id).bind(input.ref_name).bind(input.commit_sha).bind(input.sarif).bind(input.checkout_uri).bind(chrono::Utc::now()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(upload)
}

pub async fn upload_status(
    pool: &SqlitePool,
    repo: i64,
    id: &str,
) -> Result<Option<crate::code_scanning::UploadStatus>> {
    Ok(sqlx::query_as(
        "SELECT processing_status,error FROM code_scanning_uploads WHERE repo_id=$1 AND id=$2",
    )
    .bind(repo)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn pending(pool: &SqlitePool) -> Result<Option<Upload>> {
    let claim = uuid::Uuid::new_v4().to_string();
    Ok(sqlx::query_as("UPDATE code_scanning_uploads SET claim_id=$1,claim_until=strftime('%Y-%m-%dT%H:%M:%f000Z','now','+10 minutes') WHERE sequence=(SELECT sequence FROM code_scanning_uploads WHERE processing_status='pending' AND (claim_until IS NULL OR claim_until<=strftime('%Y-%m-%dT%H:%M:%f000Z','now')) ORDER BY sequence LIMIT 1) RETURNING *")
        .bind(claim).fetch_optional(pool).await?)
}

pub async fn finish(
    pool: &SqlitePool,
    upload: &Upload,
    runs: &[Run],
    error: Option<&str>,
) -> Result<Vec<Change>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let status: Option<String> = sqlx::query_scalar(
        "SELECT processing_status FROM code_scanning_uploads WHERE id=$1 AND claim_id IS $2",
    )
    .bind(&upload.id)
    .bind(&upload.claim_id)
    .fetch_optional(&mut *tx)
    .await?;
    if status.as_deref() != Some("pending") {
        return Ok(Vec::new());
    }
    let before: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,state FROM code_scanning_alerts WHERE repo_id=$1")
            .bind(upload.repo_id)
            .fetch_all(&mut *tx)
            .await?;
    let now = chrono::Utc::now();
    if error.is_none() {
        sqlx::query("INSERT INTO code_scanning_alert_counters(repo_id) VALUES($1) ON CONFLICT(repo_id) DO NOTHING").bind(upload.repo_id).execute(&mut *tx).await?;
        for run in runs {
            let analysis: i64 = sqlx::query_scalar("INSERT INTO code_scanning_analyses(repo_id,upload_id,upload_sequence,ref_name,commit_sha,tool_key,tool,category,results_count,rules_count,sarif,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING id")
                .bind(upload.repo_id).bind(&upload.id).bind(upload.sequence).bind(&upload.ref_name).bind(&upload.commit_sha).bind(&run.tool_key).bind(&run.tool).bind(&run.category).bind(run.results_count).bind(run.rules_count).bind(&run.sarif).bind(now).fetch_one(&mut *tx).await?;
            // A late worker must not overwrite a newer upload on this ref and category.
            let newer: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM code_scanning_analyses WHERE repo_id=$1 AND ref_name=$2 AND tool_key=$3 AND category=$4 AND upload_sequence>$5)")
                .bind(upload.repo_id).bind(&upload.ref_name).bind(&run.tool_key).bind(&run.category).bind(upload.sequence).fetch_one(&mut *tx).await?;
            if newer {
                continue;
            }
            sqlx::query("UPDATE code_scanning_instances SET state='fixed',upload_sequence=$5 WHERE ref_name=$1 AND category=$2 AND alert_id IN (SELECT id FROM code_scanning_alerts WHERE repo_id=$3 AND tool_key=$4) AND state='open'")
                .bind(&upload.ref_name).bind(&run.category).bind(upload.repo_id).bind(&run.tool_key).bind(upload.sequence).execute(&mut *tx).await?;
            for finding in &run.findings {
                let alert = save_alert(&mut tx, upload, run, finding, now).await?;
                sqlx::query("INSERT INTO code_scanning_analysis_findings(analysis_id,alert_id,message,location,result_index,rule) VALUES($1,$2,$3,$4,$5,$6)").bind(analysis).bind(alert.id).bind(&finding.message).bind(&finding.location).bind(finding.source_index).bind(&finding.rule).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO code_scanning_instances(alert_id,ref_name,category,commit_sha,state,message,location,upload_sequence) VALUES($1,$2,$3,$4,'open',$5,$6,$7) ON CONFLICT(alert_id,ref_name,category) DO UPDATE SET commit_sha=excluded.commit_sha,state='open',message=excluded.message,location=excluded.location,upload_sequence=excluded.upload_sequence")
                    .bind(alert.id).bind(&upload.ref_name).bind(&run.category).bind(&upload.commit_sha).bind(&finding.message).bind(&finding.location).bind(upload.sequence).execute(&mut *tx).await?;
            }
        }
        sqlx::query("UPDATE code_scanning_alerts SET state=CASE WHEN EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=code_scanning_alerts.id AND i.state='open') THEN 'open' ELSE 'fixed' END,fixed_at=CASE WHEN EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=code_scanning_alerts.id AND i.state='open') THEN NULL ELSE COALESCE(fixed_at,$2) END,updated_at=$2 WHERE repo_id=$1 AND state<>'dismissed' AND id IN(SELECT alert_id FROM code_scanning_instances WHERE upload_sequence=$3)")
            .bind(upload.repo_id).bind(now).bind(upload.sequence).execute(&mut *tx).await?;
    }
    sqlx::query(
        "UPDATE code_scanning_uploads SET processing_status=$2,error=$3,sarif='' WHERE id=$1",
    )
    .bind(&upload.id)
    .bind(if error.is_some() {
        "failed"
    } else {
        "complete"
    })
    .bind(error)
    .execute(&mut *tx)
    .await?;
    let after = sqlx::query_as("SELECT * FROM code_scanning_alerts WHERE repo_id=$1 AND id IN(SELECT alert_id FROM code_scanning_instances WHERE upload_sequence=$2)")
        .bind(upload.repo_id).bind(upload.sequence)
        .fetch_all(&mut *tx)
        .await?;
    let instances = sqlx::query_as("SELECT i.* FROM code_scanning_instances i JOIN code_scanning_alerts a ON a.id=i.alert_id WHERE a.repo_id=$1 AND i.upload_sequence=$2 AND i.ref_name=$3 ORDER BY i.category")
        .bind(upload.repo_id).bind(upload.sequence).bind(&upload.ref_name).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(changes(&before, after, instances))
}

async fn save_alert(
    connection: &mut SqliteConnection,
    upload: &Upload,
    run: &Run,
    finding: &Finding,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Alert> {
    let alert: Option<Alert> = sqlx::query_as("INSERT INTO code_scanning_alerts(repo_id,number,fingerprint,tool_key,tool,rule,created_at,updated_at) SELECT $1,next_number,$2,$3,$4,$5,$6,$6 FROM code_scanning_alert_counters WHERE repo_id=$1 ON CONFLICT(repo_id,tool_key,fingerprint) DO NOTHING RETURNING *")
        .bind(upload.repo_id).bind(&finding.fingerprint).bind(&run.tool_key).bind(&run.tool).bind(&finding.rule).bind(now).fetch_optional(&mut *connection).await?;
    if let Some(alert) = alert {
        sqlx::query(
            "UPDATE code_scanning_alert_counters SET next_number=next_number+1 WHERE repo_id=$1",
        )
        .bind(upload.repo_id)
        .execute(connection)
        .await?;
        return Ok(alert);
    }
    Ok(sqlx::query_as("UPDATE code_scanning_alerts SET rule=$4,tool=$5,updated_at=$6 WHERE repo_id=$1 AND tool_key=$2 AND fingerprint=$3 RETURNING *")
        .bind(upload.repo_id).bind(&run.tool_key).bind(&finding.fingerprint).bind(&finding.rule).bind(&run.tool).bind(now).fetch_one(connection).await?)
}

pub async fn analysis_alerts(pool: &SqlitePool, analysis: i64) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as("SELECT f.result_index,a.number FROM code_scanning_analysis_findings f JOIN code_scanning_alerts a ON a.id=f.alert_id WHERE f.analysis_id=$1 ORDER BY f.result_index").bind(analysis).fetch_all(pool).await?)
}

pub async fn alert(pool: &SqlitePool, repo: i64, number: i64) -> Result<Option<Alert>> {
    Ok(
        sqlx::query_as("SELECT * FROM code_scanning_alerts WHERE repo_id=$1 AND number=$2")
            .bind(repo)
            .bind(number)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn instance(
    pool: &SqlitePool,
    alert: i64,
    reference: Option<&str>,
) -> Result<Option<Instance>> {
    instance_filtered(pool, alert, reference, None).await
}

pub async fn instance_filtered(
    pool: &SqlitePool,
    alert: i64,
    reference: Option<&str>,
    pull_request: Option<i32>,
) -> Result<Option<Instance>> {
    let mut connection = pool.acquire().await?;
    instance_on(&mut connection, alert, reference, pull_request).await
}

async fn instance_on(
    connection: &mut SqliteConnection,
    alert: i64,
    reference: Option<&str>,
    pull_request: Option<i32>,
) -> Result<Option<Instance>> {
    ensure!(
        pull_request.is_none_or(|number| number > 0),
        "Invalid pull request"
    );
    let condition = instance_condition("$2", "$3", "i.ref_name");
    Ok(sqlx::query_as(&format!("SELECT i.* FROM code_scanning_instances i WHERE i.alert_id=$1 AND {condition} ORDER BY CASE WHEN i.state='open' THEN 0 ELSE 1 END,i.upload_sequence DESC,i.ref_name,i.category LIMIT 1")).bind(alert).bind(reference).bind(pull_request).fetch_optional(connection).await?)
}

pub async fn organization_alert_page(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    filters: &AlertFilters<'_>,
    offset: i64,
    limit: i64,
) -> Result<Vec<OrganizationAlert>> {
    let mut tx = pool.begin().await?;
    let role = codeyard_db_core::queries::sqlite::organizations::authority::read_authority(
        &mut tx, actor, org,
    )
    .await?;
    ensure!(
        matches!(
            role,
            codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(
                codeyard_db_core::models::OrgRole::Owner | codeyard_db_core::models::OrgRole::Admin
            )
        ),
        Refusal::Denied
    );
    let rows = list::alert_page_on(
        &mut tx,
        AlertScope::Organization(org),
        filters,
        offset,
        limit,
    )
    .await?;
    let mut result = Vec::with_capacity(rows.len());
    for alert in rows {
        let repository: codeyard_db_core::models::Repository =
            sqlx::query_as("SELECT * FROM repositories WHERE id=$1")
                .bind(alert.repo_id)
                .fetch_one(&mut *tx)
                .await?;
        let default = (filters.pull_request.is_none())
            .then(|| format!("refs/heads/{}", repository.default_branch));
        let reference = filters.reference.or(default.as_deref());
        let instance = instance_on(&mut tx, alert.id, reference, filters.pull_request)
            .await?
            .ok_or(Refusal::Missing)?;
        let dismissed_by = if let Some(user) = alert.dismissed_by {
            sqlx::query_as("SELECT * FROM users WHERE id=$1")
                .bind(user)
                .fetch_optional(&mut *tx)
                .await?
        } else {
            None
        };
        result.push(OrganizationAlert {
            alert,
            repository,
            instance,
            dismissed_by,
        });
    }
    tx.commit().await?;
    Ok(result)
}

pub async fn instances(
    pool: &SqlitePool,
    alert: i64,
    reference: Option<&str>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Instance>> {
    instances_filtered(pool, alert, reference, None, offset, limit).await
}

pub async fn instances_filtered(
    pool: &SqlitePool,
    alert: i64,
    reference: Option<&str>,
    pull_request: Option<i32>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Instance>> {
    ensure!(
        offset >= 0 && (1..=101).contains(&limit) && pull_request.is_none_or(|number| number > 0),
        "Invalid instance page"
    );
    let condition = instance_condition("$2", "$5", "i.ref_name");
    Ok(sqlx::query_as(&format!("SELECT i.* FROM code_scanning_instances i WHERE i.alert_id=$1 AND {condition} ORDER BY i.upload_sequence DESC,i.ref_name,i.category LIMIT $3 OFFSET $4")).bind(alert).bind(reference).bind(limit).bind(offset).bind(pull_request).fetch_all(pool).await?)
}

pub async fn dismiss(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    number: i64,
    input: &Dismissal<'_>,
) -> Result<Alert> {
    input.validate()?;
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    codeyard_db_identity::sqlite::credentials::require_account(&mut tx, &credential)
        .await
        .map_err(|error| {
            if error.is::<codeyard_types::credentials::AccountRestricted>() {
                anyhow::Error::new(Refusal::Denied)
            } else {
                error
            }
        })?;
    let mut allowed =
        codeyard_db_core::queries::sqlite::discussions::lock_write_authority(&mut tx, actor, repo)
            .await?;
    if !allowed {
        let org: Option<i64> = sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=$1 AND NOT is_archived AND owner_kind='org'")
            .bind(repo).fetch_optional(&mut *tx).await?.flatten();
        if let Some(org) = org {
            allowed = codeyard_db_core::queries::sqlite::organizations::roles::manages_security(
                &mut tx, actor, org,
            )
            .await?;
        }
    }
    ensure!(
        allowed
            && codeyard_db_core::queries::sqlite::credentials::repository(
                &mut tx,
                &credential,
                repo,
                "security_events",
                true
            )
            .await?,
        Refusal::Denied
    );
    let dismissed = input.state == "dismissed";
    let now = chrono::Utc::now();
    let alert = sqlx::query_as("UPDATE code_scanning_alerts SET state=CASE WHEN $3 THEN 'dismissed' WHEN EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=code_scanning_alerts.id AND i.state='open') THEN 'open' ELSE 'fixed' END,fixed_at=CASE WHEN $3 THEN fixed_at WHEN EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=code_scanning_alerts.id AND i.state='open') THEN NULL ELSE COALESCE(fixed_at,$8) END,dismissed_by=$4,dismissed_at=$5,dismissed_reason=$6,dismissed_comment=$7,updated_at=$8 WHERE repo_id=$1 AND number=$2 RETURNING *")
        .bind(repo).bind(number).bind(dismissed).bind(dismissed.then_some(actor)).bind(dismissed.then_some(now)).bind(if dismissed {input.reason} else {None}).bind(if dismissed {input.comment} else {None}).bind(now).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(alert)
}

pub async fn delete_analysis(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    id: i64,
    confirm: bool,
) -> Result<Option<i64>> {
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_admin_authority(&mut tx, actor, repo)
            .await?
            && codeyard_db_core::queries::sqlite::credentials::repository(
                &mut tx,
                &credential,
                repo,
                "security_events",
                true
            )
            .await?,
        Refusal::Denied
    );
    let analysis: Analysis = sqlx::query_as(&format!(
        "SELECT {ANALYSIS_COLUMNS} FROM code_scanning_analyses a WHERE a.repo_id=$1 AND a.id=$2"
    ))
    .bind(repo)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Refusal::Missing)?;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM code_scanning_analyses WHERE repo_id=$1 AND ref_name=$2 AND tool_key=$3 AND category=$4 ORDER BY upload_sequence DESC,id DESC")
        .bind(repo).bind(&analysis.ref_name).bind(&analysis.tool_key).bind(&analysis.category).fetch_all(&mut *tx).await?;
    ensure!(
        ids.first() == Some(&id) && (ids.len() > 1 || confirm),
        Refusal::DeleteOrder
    );
    sqlx::query("DELETE FROM code_scanning_analyses WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE code_scanning_instances SET state='fixed' WHERE ref_name=$1 AND category=$2 AND alert_id IN (SELECT id FROM code_scanning_alerts WHERE repo_id=$3 AND tool_key=$4)")
        .bind(&analysis.ref_name).bind(&analysis.category).bind(repo).bind(&analysis.tool_key).execute(&mut *tx).await?;
    if let Some(previous) = ids.get(1) {
        sqlx::query("INSERT INTO code_scanning_instances(alert_id,ref_name,category,commit_sha,state,message,location,upload_sequence) SELECT f.alert_id,a.ref_name,a.category,a.commit_sha,'open',f.message,f.location,a.upload_sequence FROM code_scanning_analysis_findings f JOIN code_scanning_analyses a ON a.id=f.analysis_id WHERE a.id=$1 ON CONFLICT(alert_id,ref_name,category) DO UPDATE SET commit_sha=excluded.commit_sha,state='open',message=excluded.message,location=excluded.location,upload_sequence=excluded.upload_sequence")
            .bind(previous).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM code_scanning_instances WHERE ref_name=$1 AND category=$2 AND alert_id IN (SELECT id FROM code_scanning_alerts WHERE repo_id=$3 AND tool_key=$4)")
            .bind(&analysis.ref_name).bind(&analysis.category).bind(repo).bind(&analysis.tool_key).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM code_scanning_alerts WHERE repo_id=$1 AND NOT EXISTS(SELECT 1 FROM code_scanning_analysis_findings f WHERE f.alert_id=code_scanning_alerts.id)").bind(repo).execute(&mut *tx).await?;
    sqlx::query("UPDATE code_scanning_alerts SET state=CASE WHEN EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=code_scanning_alerts.id AND i.state='open') THEN 'open' ELSE 'fixed' END,fixed_at=CASE WHEN EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=code_scanning_alerts.id AND i.state='open') THEN NULL ELSE COALESCE(fixed_at,$2) END,updated_at=$2 WHERE repo_id=$1 AND state<>'dismissed'").bind(repo).bind(chrono::Utc::now()).execute(&mut *tx).await?;
    sqlx::query("UPDATE code_scanning_alerts SET rule=(SELECT f.rule FROM code_scanning_analysis_findings f JOIN code_scanning_analyses a ON a.id=f.analysis_id WHERE f.alert_id=code_scanning_alerts.id ORDER BY a.upload_sequence DESC LIMIT 1),tool=(SELECT a.tool FROM code_scanning_analysis_findings f JOIN code_scanning_analyses a ON a.id=f.analysis_id WHERE f.alert_id=code_scanning_alerts.id ORDER BY a.upload_sequence DESC LIMIT 1) WHERE repo_id=$1").bind(repo).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(ids.get(1).copied())
}
