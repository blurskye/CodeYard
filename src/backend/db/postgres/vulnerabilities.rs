use crate::backend::{
    db::vulnerabilities::*,
    dependency_graph::Snapshot,
    vulnerabilities::Advisory,
};
use anyhow::{ensure, Result};
use sqlx::PgPool;

pub async fn settings(pool: &PgPool) -> Result<Settings> {
    Ok(
        sqlx::query_as("SELECT base_url,ecosystems FROM vulnerability_settings WHERE id=1")
            .fetch_one(pool)
            .await?,
    )
}
pub async fn save_settings(
    pool: &PgPool,
    actor: i64,
    session: &str,
    settings: &Settings,
) -> Result<()> {
    settings.validate()?;
    let mut tx = pool.begin().await?;
    ensure!(
        super::users::transitions::active_admin(&mut tx, actor).await?
            && super::users::transitions::live_session(&mut tx, actor, session).await?,
        Refusal::Denied
    );
    sqlx::query(
        "UPDATE vulnerability_settings SET base_url=$1,ecosystems=$2,next_sync=$3 WHERE id=1",
    )
    .bind(&settings.base_url)
    .bind(&settings.ecosystems)
    .bind(chrono::Utc::now())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn upsert(pool: &PgPool, advisory: &Advisory) -> Result<bool> {
    let document = advisory.global_document()?;
    let mut tx = pool.begin().await?;
    let changed=sqlx::query("INSERT INTO osv_advisories(id,modified,data) VALUES($1,$2,$3) ON CONFLICT(id) DO UPDATE SET modified=excluded.modified,data=excluded.data WHERE excluded.modified>osv_advisories.modified")
        .bind(&advisory.id).bind(advisory.modified).bind(serde_json::to_string(advisory)?).execute(&mut *tx).await?.rows_affected()>0;
    if changed {
        let ghsa_id = document["ghsa_id"].as_str().unwrap();
        // A record that gained a GHSA alias replaces the catalogue entry it had under its own id.
        sqlx::query("DELETE FROM global_advisories WHERE osv_id=$1 AND ghsa_id<>$2")
            .bind(&advisory.id)
            .bind(ghsa_id)
            .execute(&mut *tx)
            .await?;
        // Another OSV record sharing the alias replaces the entry only when it is newer.
        sqlx::query("INSERT INTO global_advisories(ghsa_id,document,osv_id) VALUES($1,$2,$3) ON CONFLICT(ghsa_id) DO UPDATE SET document=excluded.document,osv_id=excluded.osv_id WHERE global_advisories.osv_id IS NULL OR global_advisories.osv_id=excluded.osv_id OR (SELECT modified FROM osv_advisories WHERE id=global_advisories.osv_id)<$4")
            .bind(ghsa_id).bind(document.to_string()).bind(&advisory.id).bind(advisory.modified).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM osv_packages WHERE advisory=$1")
            .bind(&advisory.id)
            .execute(&mut *tx)
            .await?;
        for affected in &advisory.affected {
            let name = crate::backend::vulnerabilities::name(
                &affected.package.ecosystem,
                &affected.package.name,
            );
            sqlx::query("INSERT INTO osv_packages(advisory,ecosystem,name) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(&advisory.id).bind(&affected.package.ecosystem).bind(name).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(changed)
}
/// Moves every repository's scan forward after the advisory catalogue changed.
pub async fn rescan_all(pool: &PgPool) -> Result<()> {
    sqlx::query("UPDATE vulnerability_jobs SET generation=generation+1,lease_until=NULL,next_scan=$1")
        .bind(chrono::Utc::now())
        .execute(pool)
        .await?;
    Ok(())
}
pub async fn advisories(pool: &PgPool, ecosystem: &str, name: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT a.data FROM osv_advisories a JOIN osv_packages p ON p.advisory=a.id WHERE p.ecosystem=$1 AND p.name=$2 ORDER BY a.id").bind(ecosystem).bind(name).fetch_all(pool).await?)
}
pub async fn enqueue(pool: &PgPool, repo: i64) -> Result<()> {
    sqlx::query("INSERT INTO vulnerability_jobs(repo_id,generation,next_scan) SELECT $1,1,$2 WHERE EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND NOT is_gist) ON CONFLICT(repo_id) DO UPDATE SET generation=vulnerability_jobs.generation+1,lease_until=NULL,next_scan=$2").bind(repo).bind(chrono::Utc::now()).execute(pool).await?;
    Ok(())
}
pub async fn queue_changed(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM vulnerability_jobs WHERE repo_id IN (SELECT id FROM repositories WHERE is_gist)").execute(pool).await?;
    sqlx::query("INSERT INTO vulnerability_jobs(repo_id,generation,next_scan) SELECT id,1,$1 FROM repositories WHERE NOT is_gist ON CONFLICT(repo_id) DO NOTHING").bind(chrono::Utc::now()).execute(pool).await?;
    Ok(())
}
pub async fn claim(pool: &PgPool) -> Result<Option<(i64, i64)>> {
    let now = chrono::Utc::now();
    let mut tx = pool.begin().await?;
    let mut row:Option<(i64,i64)>=sqlx::query_as("SELECT repo_id,generation FROM vulnerability_jobs WHERE EXISTS(SELECT 1 FROM repositories WHERE id=repo_id AND NOT is_gist) AND next_scan<=$1 AND (lease_until IS NULL OR lease_until<$1) ORDER BY next_scan,repo_id LIMIT 1 FOR UPDATE SKIP LOCKED").bind(now).fetch_optional(&mut *tx).await?;
    if let Some((repo, _)) = row {
        // Moving next_scan with the lease keeps a job that never completes from staying
        // first in line ahead of newer work.
        sqlx::query("UPDATE vulnerability_jobs SET generation=generation+1,lease_until=$2,next_scan=$2 WHERE repo_id=$1")
            .bind(repo)
            .bind(now + chrono::Duration::minutes(10))
            .execute(&mut *tx)
            .await?;
    }
    if let Some((_, generation)) = row.as_mut() {
        *generation += 1;
    }
    tx.commit().await?;
    Ok(row)
}
pub async fn claim_sync(pool: &PgPool) -> Result<bool> {
    let now = chrono::Utc::now();
    Ok(sqlx::query("UPDATE vulnerability_settings SET next_sync=$2 WHERE id=1 AND next_sync<=$1 AND ecosystems<>'[]'").bind(now).bind(now+chrono::Duration::hours(6)).execute(pool).await?.rows_affected()>0)
}
pub async fn save_snapshot(
    pool: &PgPool,
    repo: i64,
    generation: i64,
    snapshot: &Snapshot,
) -> Result<()> {
    snapshot.validate_native()?;
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let branch: String =
        sqlx::query_scalar("SELECT default_branch FROM repositories WHERE id=$1 FOR NO KEY UPDATE")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    let current: Option<i64> =
        sqlx::query_scalar("SELECT generation FROM vulnerability_jobs WHERE repo_id=$1 FOR UPDATE")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    if current != Some(generation) || snapshot.reference != format!("refs/heads/{branch}") {
        return Ok(());
    }
    sqlx::query("INSERT INTO dependency_snapshots(repo_id,sha,reference,correlator,detector,scanned,body) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(repo).bind(&snapshot.sha).bind(&snapshot.reference).bind(&snapshot.job.correlator)
        .bind(&snapshot.detector.name).bind(snapshot.scanned).bind(serde_json::to_string(snapshot)?).execute(&mut *tx).await?;
    super::dependency_graph::drop_replaced_snapshots(&mut tx, repo, snapshot).await?;
    super::dependabot_alerts::refresh(&mut tx, repo).await?;
    sqlx::query("UPDATE vulnerability_jobs SET lease_until=NULL,next_scan=$3 WHERE repo_id=$1 AND generation=$2")
        .bind(repo).bind(generation).bind(chrono::Utc::now()+chrono::Duration::minutes(5)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
