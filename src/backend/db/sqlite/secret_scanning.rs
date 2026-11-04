pub(crate) mod patterns;
use crate::backend::db::secret_scanning::*;
use crate::backend::git::secret_scanning::Location;
use anyhow::{ensure, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

const PROVIDER: &str = "CASE WHEN secret_type LIKE 'custom-pattern-%' THEN 'custom' WHEN instr(secret_type,'_')=0 THEN secret_type ELSE substr(secret_type,1,instr(secret_type,'_')-1) END";

pub async fn protection(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    hash: &str,
    kind: &str,
) -> Result<Option<Bypass>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let protected: bool = sqlx::query_scalar("SELECT COALESCE((SELECT push_protection FROM secret_scanning_settings WHERE repo_id=$1),false)").bind(repo).fetch_one(&mut *tx).await?;
    if !protected {
        return Ok(None);
    }
    let now = chrono::Utc::now();
    let granted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM secret_scanning_bypasses WHERE repo_id=$1 AND actor_id=$2 AND secret_hash=$3 AND reason IS NOT NULL AND expires_at>$4)").bind(repo).bind(actor).bind(hash).bind(now).fetch_one(&mut *tx).await?;
    if granted {
        return Ok(None);
    }
    let existing: Option<Bypass> = sqlx::query_as("SELECT * FROM secret_scanning_bypasses WHERE repo_id=$1 AND actor_id=$2 AND secret_hash=$3 AND token_type=$4 AND reason IS NULL AND used_at IS NULL AND expires_at>$5 ORDER BY expires_at DESC,id LIMIT 1")
        .bind(repo).bind(actor).bind(hash).bind(kind).bind(now).fetch_optional(&mut *tx).await?;
    let placeholder = match existing {
        Some(placeholder) => placeholder,
        None => sqlx::query_as("INSERT INTO secret_scanning_bypasses(id,repo_id,actor_id,secret_hash,token_type,expires_at) VALUES($1,$2,$3,$4,$5,$6) RETURNING *")
            .bind(uuid::Uuid::new_v4().to_string()).bind(repo).bind(actor).bind(hash).bind(kind).bind(now + chrono::Duration::hours(3)).fetch_one(&mut *tx).await?,
    };
    tx.commit().await?;
    Ok(Some(placeholder))
}

pub async fn prune_expired_bypasses(pool: &SqlitePool) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM secret_scanning_bypasses WHERE id IN (SELECT id FROM secret_scanning_bypasses WHERE used_at IS NULL AND expires_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') ORDER BY expires_at,id LIMIT 1000) AND used_at IS NULL")
        .execute(pool).await?.rows_affected())
}

pub async fn bypass(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    id: &str,
    reason: Option<&str>,
) -> Result<Bypass> {
    ensure!(
        reason.is_none_or(|reason| matches!(
            reason,
            "false_positive" | "used_in_tests" | "will_fix_later"
        )),
        Refusal::Invalid
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let protected: bool = sqlx::query_scalar("SELECT COALESCE((SELECT push_protection FROM secret_scanning_settings WHERE repo_id=$1),false)").bind(repo).fetch_one(&mut *tx).await?;
    ensure!(protected, Refusal::Missing);
    let now = chrono::Utc::now();
    let placeholder: Bypass = sqlx::query_as("SELECT * FROM secret_scanning_bypasses WHERE id=$1 AND repo_id=$2 AND actor_id=$3 AND expires_at>$4")
        .bind(id).bind(repo).bind(actor).bind(now).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    let placeholder = if let Some(reason) = reason {
        sqlx::query_as(
            "UPDATE secret_scanning_bypasses SET reason=$2,expires_at=$3 WHERE id=$1 RETURNING *",
        )
        .bind(id)
        .bind(reason)
        .bind(now + chrono::Duration::hours(3))
        .fetch_one(&mut *tx)
        .await?
    } else {
        placeholder
    };
    tx.commit().await?;
    Ok(placeholder)
}

async fn org_authority(tx: &mut Transaction<'_, Sqlite>, actor: i64, org: i64) -> Result<()> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM organizations WHERE id=$1")
        .bind(org)
        .fetch_optional(&mut **tx)
        .await?;
    ensure!(exists.is_some(), Refusal::Missing);
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.role IN ('owner','admin') AND NOT u.is_banned AND NOT u.must_change_password AND EXISTS(SELECT 1 FROM org_memberships owners JOIN users active ON active.id=owners.user_id WHERE owners.org_id=m.org_id AND owners.role='owner' AND NOT active.is_banned AND NOT active.must_change_password))").bind(org).bind(actor).fetch_one(&mut **tx).await?;
    let allowed = allowed || super::organizations::roles::manages_security(tx, actor, org).await?;
    ensure!(allowed, Refusal::Denied);
    Ok(())
}

pub async fn org_alerts(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    filter: &Filter,
) -> Result<Vec<Alert>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    org_authority(&mut tx, actor, org).await?;
    let rows = sqlx::query_as(&format!("SELECT * FROM secret_scanning_alerts WHERE repo_id IN (SELECT r.id FROM repositories r JOIN secret_scanning_settings s ON s.repo_id=r.id WHERE r.owner_org_id=$1 AND s.enabled) AND ($2 IS NULL OR number=$2) AND ($3 IS NULL OR state=$3) AND ($4 IS NULL OR instr(','||$4||',',','||secret_type||',')>0) AND ($5 IS NULL OR instr(','||$5||',',','||resolution||',')>0) AND ($10 IS NULL OR instr(','||$10||',',','||secret_type||',')=0) AND ($11 IS NULL OR instr(','||$11||',',','||COALESCE(validity,'unknown')||',')>0) AND ($12 IS NULL OR ($12='*' AND assigned_to IS NOT NULL) OR ($12='none' AND assigned_to IS NULL) OR assigned_to IN (SELECT id FROM users WHERE username=$12)) AND ($13 IS NULL OR EXISTS(SELECT 1 FROM secret_scanning_bypasses b WHERE b.repo_id=secret_scanning_alerts.repo_id AND b.secret_hash=secret_scanning_alerts.secret_hash AND b.used_at IS NOT NULL)=$13) AND ($14 IS NULL OR instr(','||$14||',',','||({PROVIDER})||',')>0) AND ($15 IS NULL OR instr(','||$15||',',','||({PROVIDER})||',')=0) ORDER BY CASE WHEN $6 AND $7 THEN updated_at WHEN $6 THEN created_at END ASC, CASE WHEN NOT $6 AND $7 THEN updated_at WHEN NOT $6 THEN created_at END DESC, repo_id,number DESC LIMIT $8 OFFSET $9"))
        .bind(org).bind(filter.number).bind(filter.state.as_deref()).bind(filter.secret_type.as_deref()).bind(filter.resolution.as_deref()).bind(filter.ascending).bind(filter.updated).bind(filter.limit).bind(filter.offset).bind(filter.exclude_secret_types.as_deref()).bind(filter.validity.as_deref()).bind(filter.assignee.as_deref()).bind(filter.is_bypassed).bind(filter.providers.as_deref()).bind(filter.exclude_providers.as_deref()).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn use_bypass(
    pool: &SqlitePool,
    repo: i64,
    actor: i64,
    hash: &str,
    accepted: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bypass:Option<Bypass>=sqlx::query_as("UPDATE secret_scanning_bypasses SET used_at=COALESCE(used_at,$4) WHERE id=(SELECT id FROM secret_scanning_bypasses WHERE repo_id=$1 AND actor_id=$2 AND secret_hash=$3 AND reason IS NOT NULL AND expires_at>$4 AND expires_at<=$5 ORDER BY expires_at DESC LIMIT 1) RETURNING *").bind(repo).bind(actor).bind(hash).bind(accepted).bind(accepted+chrono::Duration::hours(3)).fetch_optional(&mut *tx).await?;
    if let Some(bypass) = bypass.filter(|bypass| {
        matches!(
            bypass.reason.as_deref(),
            Some("false_positive" | "used_in_tests")
        )
    }) {
        sqlx::query("UPDATE secret_scanning_alerts SET state='resolved',resolution=$3,resolved_by=$4,resolved_at=$5,updated_at=$5 WHERE repo_id=$1 AND secret_hash=$2 AND state='open'").bind(repo).bind(hash).bind(bypass.reason).bind(actor).bind(accepted).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn alert_bypasses(pool: &SqlitePool, keys: &[(i64, String)]) -> Result<Vec<Bypass>> {
    ensure!(keys.len() <= 100, "Too many alert enrichments");
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as("SELECT ranked.* FROM (SELECT b.*,row_number() OVER (PARTITION BY repo_id,secret_hash ORDER BY used_at DESC,id DESC) AS position FROM secret_scanning_bypasses b WHERE used_at IS NOT NULL AND (repo_id,secret_hash) IN (SELECT json_extract(value,'$[0]'),json_extract(value,'$[1]') FROM json_each($1))) ranked WHERE position=1")
        .bind(serde_json::to_string(keys)?).fetch_all(pool).await?)
}

pub async fn settings(pool: &SqlitePool, repo: i64) -> Result<Settings> {
    Ok(sqlx::query_as(
        "SELECT enabled,push_protection FROM secret_scanning_settings WHERE repo_id=$1",
    )
    .bind(repo)
    .fetch_optional(pool)
    .await?
    .unwrap_or_default())
}

async fn authorize(tx: &mut Transaction<'_, Sqlite>, actor: i64, repo: i64) -> Result<()> {
    if !super::discussions::lock_archive_authority(tx, actor, repo).await? {
        // The owning organization's security managers handle alerts without administering
        // the repository.
        let org = sqlx::query_scalar::<_, Option<i64>>("SELECT owner_org_id FROM repositories WHERE id=$1 AND owner_kind='org' AND NOT EXISTS(SELECT 1 FROM repository_migration_locks l WHERE l.repo_id=repositories.id)")
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
        let manager = match org {
            Some(org) => super::organizations::roles::manages_security(tx, actor, org).await?,
            None => false,
        };
        ensure!(manager, Refusal::Denied);
    }
    let enabled: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT enabled FROM secret_scanning_settings WHERE repo_id=$1),false)",
    )
    .bind(repo)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(enabled, Refusal::Missing);
    Ok(())
}

pub async fn save_settings(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    settings: &Settings,
) -> Result<()> {
    ensure!(
        !settings.push_protection || settings.enabled,
        Refusal::Invalid
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        super::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    // An enforced organization configuration fixes the settings it sets.
    let configured: Option<String> = sqlx::query_scalar("SELECT c.settings FROM code_security_configurations c JOIN repository_security_settings s ON s.configuration_id=c.id JOIN repositories r ON r.id=s.repo_id AND r.owner_org_id=c.org_id WHERE r.id=$1")
        .bind(repo)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(configured) = configured {
        if let Some(fixed) =
            crate::backend::db::code_security::enforced_scanning(&configured, (*settings).clone())?
        {
            ensure!(
                fixed.enabled == settings.enabled
                    && fixed.push_protection == settings.push_protection,
                Refusal::Denied
            );
        }
    }
    save_settings_on_connection(&mut tx, Some(actor), repo, settings).await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn save_settings_on_connection(
    connection: &mut sqlx::SqliteConnection,
    actor: Option<i64>,
    repo: i64,
    settings: &Settings,
) -> Result<()> {
    ensure!(
        !settings.push_protection || settings.enabled,
        Refusal::Invalid
    );
    let previous: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT enabled FROM secret_scanning_settings WHERE repo_id=$1),false)",
    )
    .bind(repo)
    .fetch_one(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO secret_scanning_settings(repo_id,enabled,push_protection) VALUES($1,$2,$3) ON CONFLICT(repo_id) DO UPDATE SET enabled=$2,push_protection=$3")
        .bind(repo).bind(settings.enabled).bind(settings.push_protection).execute(&mut *connection).await?;
    if settings.enabled && !previous {
        sqlx::query(
            "INSERT INTO secret_scanning_jobs(repo_id,scan_type,actor_id) VALUES($1,'backfill',$2)",
        )
        .bind(repo)
        .bind(actor)
        .execute(connection)
        .await?;
    }
    Ok(())
}

pub async fn alerts(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    filter: &Filter,
) -> Result<Vec<Alert>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, repo).await?;
    let rows = sqlx::query_as(&format!("SELECT * FROM secret_scanning_alerts WHERE repo_id=$1 AND ($2 IS NULL OR number=$2) AND ($3 IS NULL OR state=$3) AND ($4 IS NULL OR instr(','||$4||',',','||secret_type||',')>0) AND ($5 IS NULL OR instr(','||$5||',',','||resolution||',')>0) AND ($10 IS NULL OR instr(','||$10||',',','||secret_type||',')=0) AND ($11 IS NULL OR instr(','||$11||',',','||COALESCE(validity,'unknown')||',')>0) AND ($12 IS NULL OR ($12='*' AND assigned_to IS NOT NULL) OR ($12='none' AND assigned_to IS NULL) OR assigned_to IN (SELECT id FROM users WHERE username=$12)) AND ($13 IS NULL OR EXISTS(SELECT 1 FROM secret_scanning_bypasses b WHERE b.repo_id=secret_scanning_alerts.repo_id AND b.secret_hash=secret_scanning_alerts.secret_hash AND b.used_at IS NOT NULL)=$13) AND ($14 IS NULL OR instr(','||$14||',',','||({PROVIDER})||',')>0) AND ($15 IS NULL OR instr(','||$15||',',','||({PROVIDER})||',')=0) ORDER BY CASE WHEN $6 AND $7 THEN updated_at WHEN $6 THEN created_at END ASC, CASE WHEN NOT $6 AND $7 THEN updated_at WHEN NOT $6 THEN created_at END DESC, number DESC LIMIT $8 OFFSET $9"))
        .bind(repo).bind(filter.number).bind(filter.state.as_deref()).bind(filter.secret_type.as_deref()).bind(filter.resolution.as_deref()).bind(filter.ascending).bind(filter.updated).bind(filter.limit).bind(filter.offset).bind(filter.exclude_secret_types.as_deref()).bind(filter.validity.as_deref()).bind(filter.assignee.as_deref()).bind(filter.is_bypassed).bind(filter.providers.as_deref()).bind(filter.exclude_providers.as_deref()).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn locations(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    number: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Location>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, repo).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM secret_scanning_alerts WHERE repo_id=$1 AND number=$2)",
    )
    .bind(repo)
    .bind(number)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(exists, Refusal::Missing);
    let rows = sqlx::query_as("SELECT * FROM secret_scanning_locations WHERE repo_id=$1 AND alert_number=$2 ORDER BY commit_sha,path,start_line,start_column LIMIT $3 OFFSET $4")
        .bind(repo).bind(number).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn update_alert(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    number: i64,
    input: &AlertUpdate<'_>,
) -> Result<Alert> {
    ensure!(
        input.state.is_some() || input.assignee.is_some() || input.validity.is_some(),
        Refusal::Invalid
    );
    ensure!(
        input
            .state
            .is_none_or(|state| matches!(state, "open" | "resolved")),
        Refusal::Invalid
    );
    ensure!(
        if input.state == Some("resolved") {
            matches!(
                input.reason,
                Some("false_positive" | "wont_fix" | "revoked" | "used_in_tests")
            )
        } else {
            input.reason.is_none()
        },
        Refusal::Invalid
    );
    ensure!(
        input
            .comment
            .is_none_or(|text| input.state.is_some() && text.len() <= 280 && !text.contains('\0')),
        Refusal::Invalid
    );
    ensure!(
        input
            .validity
            .flatten()
            .is_none_or(|validity| matches!(validity, "active" | "inactive")),
        Refusal::Invalid
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, repo).await?;
    let previous: Alert =
        sqlx::query_as("SELECT * FROM secret_scanning_alerts WHERE repo_id=$1 AND number=$2")
            .bind(repo)
            .bind(number)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    let mut assigned_to = previous.assigned_to;
    if let Some(assignee) = input.assignee {
        assigned_to = if let Some(login) = assignee {
            let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username=$1")
                .bind(login)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Refusal::Invalid)?;
            ensure!(
                super::repositories::access::user_repo_access_in_transaction(&mut tx, id, repo)
                    .await?
                    .can_write(),
                Refusal::Invalid
            );
            Some(id)
        } else {
            None
        };
    }
    let now = chrono::Utc::now();
    let state = input.state.unwrap_or(&previous.state);
    let resolution = if input.state.is_some() {
        input.reason
    } else {
        previous.resolution.as_deref()
    };
    let comment = if input.state.is_some() {
        input.comment
    } else {
        previous.resolution_comment.as_deref()
    };
    let resolved_by = if input.state.is_some() {
        (state == "resolved").then_some(actor)
    } else {
        previous.resolved_by
    };
    let resolved_at = if input.state.is_some() {
        (state == "resolved").then_some(now)
    } else {
        previous.resolved_at
    };
    let validity = input.validity.unwrap_or(previous.validity.as_deref());
    let row=sqlx::query_as("UPDATE secret_scanning_alerts SET state=$3,resolution=$4,resolution_comment=$5,resolved_by=$6,resolved_at=$7,updated_at=$8,assigned_to=$9,validity=$10 WHERE repo_id=$1 AND number=$2 RETURNING *")
        .bind(repo).bind(number).bind(state).bind(resolution).bind(comment).bind(resolved_by).bind(resolved_at).bind(now).bind(assigned_to).bind(validity).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn record(
    pool: &SqlitePool,
    repo: i64,
    input: &Finding<'_>,
) -> Result<Option<(Alert, bool, bool)>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let settings: Option<(bool, i64)> =
        sqlx::query_as("SELECT enabled,next_number FROM secret_scanning_settings WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((true, next)) = settings else {
        return Ok(None);
    };
    // A scan that started before its custom pattern was deleted does not bring the alerts back.
    if let Some(pattern) = input.custom_pattern() {
        let live: Option<i64> =
            sqlx::query_scalar("SELECT id FROM secret_scanning_patterns WHERE id=$1")
                .bind(pattern)
                .fetch_optional(&mut *tx)
                .await?;
        if live.is_none() {
            return Ok(None);
        }
    }
    // Each pattern that finds the secret keeps its own alert, so deleting one pattern leaves
    // the others' alerts in place.
    let previous: Option<Alert> = sqlx::query_as(
        "SELECT * FROM secret_scanning_alerts WHERE repo_id=$1 AND secret_hash=$2 AND secret_type=$3",
    )
    .bind(repo)
    .bind(input.hash)
    .bind(input.secret_type)
    .fetch_optional(&mut *tx)
    .await?;
    let created = previous.is_none();
    let alert = if let Some(alert) = previous {
        alert
    } else {
        let bypass:Option<Bypass>=sqlx::query_as("SELECT * FROM secret_scanning_bypasses WHERE repo_id=$1 AND secret_hash=$2 AND used_at IS NOT NULL AND reason IN ('false_positive','used_in_tests') ORDER BY used_at DESC LIMIT 1").bind(repo).bind(input.hash).fetch_optional(&mut *tx).await?;
        let alert: Alert = sqlx::query_as("INSERT INTO secret_scanning_alerts(repo_id,number,secret_hash,secret_type,display_name,encrypted_secret,nonce,state,resolution,resolved_by,resolved_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *")
            .bind(repo).bind(next).bind(input.hash).bind(input.secret_type).bind(input.display_name).bind(input.encrypted_secret).bind(input.nonce).bind(if bypass.is_some(){"resolved"}else{"open"}).bind(bypass.as_ref().and_then(|bypass|bypass.reason.as_deref())).bind(bypass.as_ref().map(|bypass|bypass.actor_id)).bind(bypass.as_ref().and_then(|bypass|bypass.used_at)).fetch_one(&mut *tx).await?;
        sqlx::query(
            "UPDATE secret_scanning_settings SET next_number=next_number+1 WHERE repo_id=$1",
        )
        .bind(repo)
        .execute(&mut *tx)
        .await?;
        alert
    };
    let location = input.location;
    let added = sqlx::query("INSERT INTO secret_scanning_locations(repo_id,alert_number,commit_sha,blob_sha,path,path_digest,start_line,end_line,start_column,end_column) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING")
        .bind(repo).bind(alert.number).bind(&location.commit_sha).bind(&location.blob_sha).bind(&location.path).bind(path_digest(&location.path)).bind(location.start_line).bind(location.end_line).bind(location.start_column).bind(location.end_column).execute(&mut *tx).await?.rows_affected() > 0;
    tx.commit().await?;
    Ok(Some((alert, created, added)))
}

pub async fn enqueue(pool: &SqlitePool, repo: i64, head: &str, actor: i64) -> Result<()> {
    sqlx::query("INSERT INTO secret_scanning_jobs(repo_id,head_sha,actor_id,scan_type) SELECT repo_id,$2,$3,'incremental' FROM secret_scanning_settings WHERE repo_id=$1 AND enabled")
        .bind(repo).bind(head).bind(actor).execute(pool).await?;
    Ok(())
}

pub async fn claim(pool: &SqlitePool) -> Result<Option<ScanJob>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(5);
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM secret_scanning_jobs WHERE (status='queued' AND (heartbeat_at IS NULL OR heartbeat_at<=$2)) OR (status='running' AND heartbeat_at<$1) ORDER BY id LIMIT 1")
        .bind(cutoff).bind(chrono::Utc::now()).fetch_optional(&mut *tx).await?;
    let Some(id) = id else { return Ok(None) };
    let job = sqlx::query_as(
        "UPDATE secret_scanning_jobs SET status='running',attempt=attempt+1,started_at=COALESCE(started_at,$2),heartbeat_at=$2 WHERE id=$1 RETURNING *",
    )
    .bind(id)
    .bind(chrono::Utc::now())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(job))
}

pub async fn finish(
    pool: &SqlitePool,
    id: i64,
    attempt: i64,
    completion: ScanCompletion,
) -> Result<()> {
    let retry = matches!(completion, ScanCompletion::Retry);
    let now = chrono::Utc::now();
    sqlx::query("UPDATE secret_scanning_jobs SET status=$2,completed_at=$3,heartbeat_at=$5 WHERE id=$1 AND attempt=$4 AND status='running'")
        .bind(id)
        .bind(completion.status())
        .bind((!retry).then_some(now))
        .bind(attempt)
        .bind(if retry { now + chrono::Duration::seconds(60) } else { now })
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn history(pool: &SqlitePool, actor: i64, repo: i64) -> Result<Vec<ScanJob>> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, repo).await?;
    let jobs = sqlx::query_as(
        "SELECT * FROM secret_scanning_jobs WHERE repo_id=$1 ORDER BY id DESC LIMIT 100",
    )
    .bind(repo)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(jobs)
}

pub async fn scanned(pool: &SqlitePool, repo: i64, commit: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM secret_scanning_commits WHERE repo_id=$1 AND commit_sha=$2 AND ancestry_complete)",
    )
    .bind(repo)
    .bind(commit)
    .fetch_one(pool)
    .await?)
}

pub async fn heartbeat(
    pool: &SqlitePool,
    repo: i64,
    job: i64,
    attempt: i64,
    scanned: Option<&str>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let held = sqlx::query(
        "UPDATE secret_scanning_jobs SET heartbeat_at=$2 WHERE id=$1 AND attempt=$3 AND status='running'",
    )
    .bind(job)
    .bind(chrono::Utc::now())
    .bind(attempt)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if held {
        if let Some(commit) = scanned {
            sqlx::query("INSERT INTO secret_scanning_commits(repo_id,commit_sha,ancestry_complete) VALUES($1,$2,TRUE) ON CONFLICT(repo_id,commit_sha) DO UPDATE SET ancestry_complete=TRUE").bind(repo).bind(commit).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(held)
}
