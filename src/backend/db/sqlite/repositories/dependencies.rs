use crate::backend::db::models::Repository;
use crate::backend::db::repositories::dependencies::{
    self as records, Feature, Job, Policy, Refusal, Snapshot, State, Update, COLUMNS, LEASE_SECONDS,
};
use anyhow::{ensure, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

async fn admin(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo: i64,
    write: bool,
) -> Result<Repository> {
    let allowed = if write {
        crate::backend::db::sqlite::discussions::lock_admin_authority(tx, actor, repo).await?
    } else {
        crate::backend::db::sqlite::discussions::lock_archive_authority(tx, actor, repo).await?
    };
    ensure!(allowed, Refusal::Denied);
    Ok(sqlx::query_as("SELECT * FROM repositories WHERE id=?1")
        .bind(repo)
        .fetch_one(&mut **tx)
        .await?)
}
async fn state_in(tx: &mut Transaction<'_, Sqlite>, repo: i64) -> Result<Option<State>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM repository_dependency_security WHERE repo_id=?1"
    ))
    .bind(repo)
    .fetch_optional(&mut **tx)
    .await?)
}
pub async fn state(pool: &SqlitePool, repo: i64) -> Result<Option<State>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM repository_dependency_security WHERE repo_id=?1"
    ))
    .bind(repo)
    .fetch_optional(pool)
    .await?)
}
async fn document(tx: &mut Transaction<'_, Sqlite>, repo: i64) -> Result<Snapshot> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT snapshot FROM repository_dependency_security WHERE repo_id=?1")
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?;
    crate::backend::db::sqlite::dependabot_alerts::refresh(tx, repo).await?;
    let rows = sqlx::query_as("SELECT * FROM dependabot_alerts WHERE repo_id=$1 ORDER BY number")
        .bind(repo)
        .fetch_all(&mut **tx)
        .await?;
    let inventory: Option<String> = sqlx::query_scalar("SELECT s.body FROM dependency_snapshots s JOIN repositories r ON r.id=s.repo_id WHERE s.repo_id=$1 AND s.reference='refs/heads/'||r.default_branch AND s.detector='Codeyard' AND s.correlator='lockfiles' ORDER BY s.scanned DESC,s.id DESC LIMIT 1")
        .bind(repo).fetch_optional(&mut **tx).await?;
    records::project(
        rows,
        inventory.map(|s| serde_json::from_str(&s)).transpose()?,
        raw.as_deref().unwrap_or("{}"),
    )
}
async fn save(tx: &mut Transaction<'_, Sqlite>, repo: i64, snapshot: &Snapshot) -> Result<()> {
    sqlx::query("UPDATE repository_dependency_security SET snapshot=?2 WHERE repo_id=?1")
        .bind(repo)
        .bind(records::receipts(snapshot)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn configure(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::OwnedAccountCredential,
    repo: i64,
    feature: Feature,
    enabled: bool,
    private_consent: bool,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repository = admin(&mut tx, actor, repo, true).await?;
    ensure!(super::super::users::transitions::live_credential(&mut tx, actor, &credential.as_credential()).await?, Refusal::Denied);
    let current = state_in(&mut tx, repo).await?;
    let Some(policy) = Policy::change(
        current.as_ref(),
        &repository,
        actor,
        feature,
        enabled,
        private_consent,
    )?
    else {
        tx.commit().await?;
        return Ok(());
    };
    if matches!(feature, Feature::Updates) && enabled {
        let authorized = match policy.alerts_actor {
            Some(id) => {
                crate::backend::db::sqlite::discussions::lock_admin_authority(&mut tx, id, repo)
                    .await?
            }
            None => false,
        };
        ensure!(authorized, Refusal::AlertsDisabled);
    }
    sqlx::query("INSERT INTO repository_dependency_security (repo_id,alerts_enabled,updates_enabled,alerts_actor_id,updates_actor_id,owner_kind,owner_id,private_consent,next_scan_at,last_status) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,CASE WHEN ?2 THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') END,CASE WHEN ?2 THEN 'queued' ELSE 'idle' END) ON CONFLICT(repo_id) DO UPDATE SET alerts_enabled=excluded.alerts_enabled,updates_enabled=excluded.updates_enabled,alerts_actor_id=excluded.alerts_actor_id,updates_actor_id=excluded.updates_actor_id,owner_kind=excluded.owner_kind,owner_id=excluded.owner_id,private_consent=excluded.private_consent,generation=repository_dependency_security.generation+1,next_scan_at=excluded.next_scan_at,lease_nonce=NULL,lease_until=NULL,attempts=0,last_status=excluded.last_status,last_error=NULL").bind(repo).bind(policy.alerts).bind(policy.updates).bind(policy.alerts_actor).bind(policy.updates_actor).bind(&policy.owner_kind).bind(policy.owner_id).bind(policy.consent).execute(&mut *tx).await?;
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT security_and_analysis FROM repository_security_settings WHERE repo_id=$1",
    )
    .bind(repo)
    .fetch_optional(&mut *tx)
    .await?;
    let mut settings: serde_json::Value = raw
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_else(|| serde_json::json!({}));
    settings["dependabot_alerts"] =
        serde_json::json!({"status": if policy.alerts {"enabled"} else {"disabled"}});
    settings["dependabot_security_updates"] =
        serde_json::json!({"status": if policy.updates {"enabled"} else {"disabled"}});
    sqlx::query("INSERT INTO repository_security_settings(repo_id,security_and_analysis) VALUES($1,$2) ON CONFLICT(repo_id) DO UPDATE SET security_and_analysis=$2").bind(repo).bind(settings.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn enqueue(pool: &SqlitePool, repo: i64) -> Result<()> {
    sqlx::query("UPDATE repository_dependency_security SET generation=generation+1,next_scan_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),last_status=CASE WHEN lease_nonce IS NULL THEN 'queued' ELSE last_status END WHERE repo_id=?1 AND alerts_enabled").bind(repo).execute(pool).await?;
    Ok(())
}
pub async fn claim(pool: &SqlitePool, nonce: &str) -> Result<Option<Job>> {
    ensure!(
        uuid::Uuid::parse_str(nonce).is_ok(),
        Refusal::Invalid("Invalid dependency job lease")
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current:Option<State>=sqlx::query_as(&format!("SELECT {COLUMNS} FROM repository_dependency_security WHERE alerts_enabled AND alerts_actor_id IS NOT NULL AND next_scan_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND (lease_until IS NULL OR lease_until<=strftime('%Y-%m-%dT%H:%M:%f000Z','now')) ORDER BY next_scan_at,repo_id LIMIT 1")).fetch_optional(&mut *tx).await?;
    let Some(current) = current else {
        tx.commit().await?;
        return Ok(None);
    };
    sqlx::query("UPDATE repository_dependency_security SET lease_nonce=?2,lease_until=strftime('%Y-%m-%dT%H:%M:%f000Z','now','+'||?3||' seconds'),attempts=MIN(attempts+1,20),last_status='scanning',last_attempt_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE repo_id=?1").bind(current.repo_id).bind(nonce).bind(LEASE_SECONDS).execute(&mut *tx).await?;
    let job = Job {
        repo_id: current.repo_id,
        generation: current.generation,
        nonce: nonce.into(),
        actor_id: current.alerts_actor_id.ok_or(Refusal::Stale)?,
        updates_actor_id: current.updates_actor_id.filter(|_| current.updates_enabled),
    };
    tx.commit().await?;
    Ok(Some(job))
}
async fn active(tx: &mut Transaction<'_, Sqlite>, job: &Job, feature: Feature) -> Result<State> {
    let repository = admin(tx, job.actor_id, job.repo_id, true).await?;
    let current = state_in(tx, job.repo_id).await?.ok_or(Refusal::Stale)?;
    ensure!(
        job.matches(&current)
            && current.same_owner(&repository)
            && (!repository.is_private || current.private_consent),
        Refusal::Stale
    );
    let valid:bool=sqlx::query_scalar("SELECT lease_until>strftime('%Y-%m-%dT%H:%M:%f000Z','now') FROM repository_dependency_security WHERE repo_id=?1").bind(job.repo_id).fetch_one(&mut **tx).await?;
    ensure!(valid, Refusal::Stale);
    let settings: Option<String> = sqlx::query_scalar(
        "SELECT security_and_analysis FROM repository_security_settings WHERE repo_id=$1",
    )
    .bind(job.repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(settings) = settings {
        let settings: serde_json::Value = serde_json::from_str(&settings)?;
        ensure!(
            settings["dependabot_alerts"]["status"] != "disabled",
            Refusal::Denied
        );
        if matches!(feature, Feature::Updates) {
            ensure!(
                settings["dependabot_security_updates"]["status"] != "disabled",
                Refusal::Denied
            );
        }
    }
    if matches!(feature, Feature::Updates) {
        let actor = job.updates_actor_id.ok_or(Refusal::Denied)?;
        ensure!(
            current.updates_enabled && current.updates_actor_id == Some(actor),
            Refusal::Stale
        );
        admin(tx, actor, job.repo_id, true).await?;
    }
    Ok(current)
}
pub async fn check_job(pool: &SqlitePool, job: &Job, feature: Feature) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    active(&mut tx, job, feature).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn snapshot(pool: &SqlitePool, actor: i64, repo: i64) -> Result<Snapshot> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    admin(&mut tx, actor, repo, false).await?;
    let snapshot = document(&mut tx, repo).await?;
    tx.commit().await?;
    Ok(snapshot)
}
pub async fn complete_scan(pool: &SqlitePool, job: &Job, sha: &str) -> Result<()> {
    ensure!(records::sha(sha), Refusal::Stale);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    active(&mut tx, job, Feature::Alerts).await?;
    let current: Option<String> = sqlx::query_scalar("SELECT s.sha FROM dependency_snapshots s JOIN repositories r ON r.id=s.repo_id WHERE s.repo_id=$1 AND s.reference='refs/heads/'||r.default_branch AND s.detector='Codeyard' AND s.correlator='lockfiles' ORDER BY s.scanned DESC,s.id DESC LIMIT 1")
        .bind(job.repo_id).fetch_optional(&mut *tx).await?;
    ensure!(current.as_deref() == Some(sha), Refusal::Stale);
    sqlx::query("UPDATE repository_dependency_security SET last_scan_sha=?2,last_scan_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),last_status='complete',last_error=NULL WHERE repo_id=?1").bind(job.repo_id).bind(sha).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
// Keep the lease after saving a scan: this job may still propose a verified patch.
pub async fn release(
    pool: &SqlitePool,
    job: &Job,
    status: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    ensure!(
        status.is_none_or(|s| matches!(s, "error" | "paused" | "unsupported"))
            && error.is_none_or(|s| s.len() <= 1024 && !s.contains('\0')),
        Refusal::Invalid("Invalid dependency job result")
    );
    sqlx::query("UPDATE repository_dependency_security SET lease_nonce=NULL,lease_until=NULL,last_status=CASE WHEN generation<>?3 THEN 'queued' ELSE COALESCE(?4,last_status) END,last_error=CASE WHEN generation<>?3 THEN NULL ELSE ?5 END,next_scan_at=CASE WHEN generation<>?3 THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHEN ?4 IS NOT NULL THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now','+'||MIN(21600,60*MAX(attempts,1)*MAX(attempts,1))||' seconds') ELSE strftime('%Y-%m-%dT%H:%M:%f000Z','now','+6 hours') END,attempts=CASE WHEN generation<>?3 OR ?4 IS NULL THEN 0 ELSE attempts END WHERE repo_id=?1 AND lease_nonce=?2").bind(job.repo_id).bind(&job.nonce).bind(job.generation).bind(status).bind(error).execute(pool).await?;
    Ok(())
}
pub async fn manual(
    pool: &SqlitePool,
    job: &Job,
    key: &str,
    version: &str,
    reason: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    active(&mut tx, job, Feature::Updates).await?;
    let mut snapshot = document(&mut tx, job.repo_id).await?;
    snapshot.manual(key, version, reason)?;
    save(&mut tx, job.repo_id, &snapshot).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn proposal_number(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    branch: &str,
) -> Result<Option<i32>> {
    let mut tx = pool.begin().await?;
    admin(&mut tx, actor, repo, false).await?;
    let number=sqlx::query_scalar("SELECT number FROM pull_requests WHERE repo_id=?1 AND head_repo_id=?1 AND head_branch=?2 ORDER BY number DESC LIMIT 1").bind(repo).bind(branch).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(number)
}
pub async fn plan_update(pool: &SqlitePool, job: &Job, key: &str, update: &Update) -> Result<()> {
    update.validate()?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current = active(&mut tx, job, Feature::Updates).await?;
    ensure!(current.scanned_in_lease(&update.base_sha), Refusal::Stale);
    ensure!(
        Some(update.requested_by) == job.updates_actor_id,
        Refusal::Denied
    );
    let mut snapshot = document(&mut tx, job.repo_id).await?;
    let alert = snapshot.alerts.get_mut(key).ok_or(Refusal::Missing)?;
    ensure!(
        alert.state == crate::backend::db::repositories::dependencies::AlertState::Open
            && alert.finding.package.version == update.old_version
            && alert.seen_sha == update.base_sha,
        Refusal::Stale
    );
    alert.update = Some(update.clone());
    alert.manual_reason = None;
    save(&mut tx, job.repo_id, &snapshot).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn attach_pull(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    key: &str,
    commit: &str,
    pull: i64,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    admin(&mut tx, actor, repo, true).await?;
    let mut snapshot = document(&mut tx, repo).await?;
    let update = snapshot
        .alerts
        .get_mut(key)
        .and_then(|a| a.update.as_mut())
        .ok_or(Refusal::Missing)?;
    ensure!(update.commit_sha == commit, Refusal::Stale);
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pull_requests WHERE id=?1 AND repo_id=?2 AND head_repo_id=?2 AND head_branch=?3 AND head_sha=?4)").bind(pull).bind(repo).bind(&update.branch).bind(commit).fetch_one(&mut *tx).await?;
    ensure!(valid, Refusal::Stale);
    update.pull_request_id = Some(pull);
    save(&mut tx, repo, &snapshot).await?;
    tx.commit().await?;
    Ok(())
}
