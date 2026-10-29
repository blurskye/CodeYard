use crate::backend::{
    db::{
        advisories::{Advisory, Refusal},
        dependabot_alerts::{self, Alert, Update},
    },
    dependency_graph::Snapshot,
};
use anyhow::{ensure, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};

/// The catalogue an inventory can match: advisories without an OSV record, and those whose
/// OSV record names one of the inventory's packages (`keys`, from `osv_keys`). Without keys it
/// is the whole catalogue. An imported document wins over a published one with the same GHSA ID.
async fn global_in(tx: &mut Transaction<'_, Postgres>, keys: Option<&str>) -> Result<Vec<Value>> {
    let imported: Vec<String> = sqlx::query_scalar(
        "SELECT document FROM global_advisories WHERE $1::jsonb IS NULL OR osv_id IS NULL OR osv_id IN (SELECT p.advisory FROM osv_packages p JOIN jsonb_array_elements($1::jsonb) j ON p.ecosystem=j->>0 AND p.name=j->>1) ORDER BY ghsa_id",
    )
    .bind(keys)
    .fetch_all(&mut **tx)
    .await?;
    let mut documents = std::collections::BTreeMap::new();
    let published: Vec<Advisory> = sqlx::query_as("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id WHERE a.state='published' AND NOT r.is_private AND NOT EXISTS (SELECT 1 FROM global_advisories g WHERE g.ghsa_id=a.ghsa_id) ORDER BY a.ghsa_id").fetch_all(&mut **tx).await?;
    for row in published {
        match dependabot_alerts::published(row) {
            Ok(value) => { documents.insert(value["ghsa_id"].as_str().unwrap().to_owned(), value); }
            Err(_) => tracing::warn!("Invalid published advisory excluded from catalogue"),
        }
    }
    for body in imported {
        let value: Value = serde_json::from_str(&body)?;
        documents.insert(value["ghsa_id"].as_str().unwrap().to_owned(), value);
    }
    Ok(documents.into_values().collect())
}
/// Page identifiers under one read snapshot without retaining the catalogue's documents.
pub async fn global_page(pool: &PgPool, query: &dependabot_alerts::CatalogueQuery) -> Result<dependabot_alerts::CataloguePage> {
    use futures_util::TryStreamExt;
    ensure!((1..=100).contains(&query.limit) && query.offset <= 1_000_000, Refusal::Invalid);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    let (statement, args, offset_applied) = dependabot_alerts::catalogue_statement(true, query);
    let mut selected = dependabot_alerts::CatalogueSelection::new(query, offset_applied);
    {
        let mut candidates = sqlx::query_as::<_, (String, String)>(&statement);
        for value in &args { candidates = candidates.bind(value); }
        let mut candidates = candidates.fetch(&mut *tx);
        while let Some((id, metadata)) = candidates.try_next().await? {
            if !selected.push(id, serde_json::from_str(&metadata)?) { break; }
        }
    }
    let (ids, previous, next) = selected.finish();
    let mut documents = Vec::with_capacity(ids.len());
    for id in ids {
        match global_one_in(&mut tx, &id).await {
            Ok(Some(value)) => documents.push(value),
            Ok(None) => (),
            Err(error) if error.downcast_ref::<Refusal>().is_some() => tracing::warn!(%id, "Invalid published advisory excluded from catalogue"),
            Err(error) => return Err(error),
        }
    }
    tx.commit().await?;
    Ok(dependabot_alerts::CataloguePage { documents, previous, next })
}

/// The whole advisory catalogue.
pub async fn global(pool: &PgPool) -> Result<Vec<Value>> {
    let mut tx = pool.begin().await?;
    let rows = global_in(&mut tx, None).await?;
    tx.commit().await?;
    Ok(rows)
}
/// The catalogue advisories that have no OSV record, which `vulnerabilities::findings` cannot
/// see: imported ones without one, and published repository advisories that no import replaces.
pub async fn without_osv(pool: &PgPool) -> Result<Vec<Value>> {
    let mut tx = pool.begin().await?;
    let imported: Vec<String> = sqlx::query_scalar(
        "SELECT document FROM global_advisories WHERE osv_id IS NULL ORDER BY ghsa_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let published: Vec<Advisory> = sqlx::query_as("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id WHERE a.state='published' AND NOT r.is_private AND NOT EXISTS (SELECT 1 FROM global_advisories g WHERE g.ghsa_id=a.ghsa_id) ORDER BY a.ghsa_id").fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let mut rows = Vec::with_capacity(imported.len() + published.len());
    for row in published {
        match dependabot_alerts::published(row) {
            Ok(value) => rows.push(value),
            Err(_) => tracing::warn!("Invalid published advisory excluded from catalogue"),
        }
    }
    for body in imported {
        rows.push(serde_json::from_str(&body)?);
    }
    Ok(rows)
}
/// One catalogue advisory by GHSA ID through the key index. An imported document wins over a published one, as in `global_in`.
pub async fn global_one(pool: &PgPool, ghsa_id: &str) -> Result<Option<Value>> {
    let mut connection = pool.acquire().await?;
    global_one_in(&mut connection, ghsa_id).await
}
async fn global_one_in(connection: &mut sqlx::PgConnection, ghsa_id: &str) -> Result<Option<Value>> {
    let imported: Option<String> =
        sqlx::query_scalar("SELECT document FROM global_advisories WHERE ghsa_id=$1")
            .bind(ghsa_id)
            .fetch_optional(&mut *connection)
            .await?;
    if let Some(body) = imported {
        return Ok(Some(serde_json::from_str(&body)?));
    }
    let published: Option<Advisory> = sqlx::query_as("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id WHERE a.state='published' AND NOT r.is_private AND a.ghsa_id=$1")
        .bind(ghsa_id)
        .fetch_optional(&mut *connection)
        .await?;
    match published {
        Some(row) => Ok(Some(dependabot_alerts::published(row)?)),
        None => Ok(None),
    }
}
pub async fn import(pool: &PgPool, actor: i64, value: &Value) -> Result<Value> {
    let value = dependabot_alerts::import_document(value)?;
    let mut tx = pool.begin().await?;
    ensure!(
        super::users::transitions::active_admin(&mut tx, actor).await?,
        Refusal::Denied
    );
    sqlx::query("INSERT INTO global_advisories(ghsa_id,document) VALUES($1,$2) ON CONFLICT(ghsa_id) DO UPDATE SET document=$2,osv_id=NULL")
        .bind(value["ghsa_id"].as_str().unwrap()).bind(value.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(value)
}
pub(crate) async fn refresh(tx: &mut Transaction<'_, Postgres>, repo: i64) -> Result<()> {
    let branch: String = sqlx::query_scalar("SELECT default_branch FROM repositories WHERE id=$1")
        .bind(repo)
        .fetch_one(&mut **tx)
        .await?;
    let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM (SELECT body,ROW_NUMBER() OVER(PARTITION BY detector,correlator ORDER BY scanned DESC,id DESC) AS position FROM dependency_snapshots WHERE repo_id=$1 AND reference=$2) ranked WHERE position=1 ORDER BY body")
        .bind(repo).bind(format!("refs/heads/{branch}")).fetch_all(&mut **tx).await?;
    let snapshots: Vec<Snapshot> = bodies
        .into_iter()
        .map(|s| serde_json::from_str(&s))
        .collect::<std::result::Result<_, _>>()?;
    for snapshot in &snapshots {
        snapshot.validate()?;
    }
    let keys = dependabot_alerts::osv_keys(&snapshots);
    let advisories = global_in(tx, Some(&keys)).await?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT g.ghsa_id,a.data FROM global_advisories g JOIN osv_advisories a ON a.id=g.osv_id WHERE g.osv_id IN (SELECT p.advisory FROM osv_packages p JOIN jsonb_array_elements($1::jsonb) j ON p.ecosystem=j->>0 AND p.name=j->>1)",
    )
    .bind(&keys)
    .fetch_all(&mut **tx)
    .await?;
    let osv = rows
        .into_iter()
        .map(|(id, data)| Ok((id, serde_json::from_str(&data)?)))
        .collect::<Result<_>>()?;
    let matches = dependabot_alerts::matches(&snapshots, &advisories, &osv);
    let now = Utc::now();
    let previous: Vec<Alert> =
        sqlx::query_as("SELECT * FROM dependabot_alerts WHERE repo_id=$1 ORDER BY number")
            .bind(repo)
            .fetch_all(&mut **tx)
            .await?;
    let matched: std::collections::HashSet<_> = matches.iter().map(|(p, a, _)| (a["ghsa_id"].as_str().unwrap(), p.manifest.as_str(), p.ecosystem.as_str(), p.name.as_str())).collect();
    let known: std::collections::HashMap<_, _> = previous.iter().map(|row| ((row.ghsa_id.as_str(), row.manifest.as_str(), row.ecosystem.as_str(), row.package.as_str()), row)).collect();
    for row in &previous {
        let found = matched.contains(&(row.ghsa_id.as_str(), row.manifest.as_str(), row.ecosystem.as_str(), row.package.as_str()));
        if !found && row.state == "open" {
            sqlx::query("UPDATE dependabot_alerts SET state='fixed',fixed_at=$3,updated_at=$3 WHERE repo_id=$1 AND number=$2").bind(repo).bind(row.number).bind(now).execute(&mut **tx).await?;
        }
    }
    drop(matched);
    let mut next = previous.last().map_or(1, |r| r.number + 1);
    for (p, a, v) in matches {
        let old = known.get(&(a["ghsa_id"].as_str().unwrap(), p.manifest.as_str(), p.ecosystem.as_str(), p.name.as_str())).copied();
        let number = old.map_or(next, |r| r.number);
        if old.is_none() {
            next += 1;
        }
        let advisory = a.to_string();
        let vulnerability = v.to_string();
        if old.is_some_and(|r| {
            r.version == p.version
                && r.scope == p.scope
                && r.advisory == advisory
                && r.vulnerability == vulnerability
                && r.state != "fixed"
        }) {
            continue;
        }
        sqlx::query("INSERT INTO dependabot_alerts(repo_id,number,ghsa_id,manifest,ecosystem,package,version,scope,advisory,vulnerability,created_at,updated_at,state,manifest_hash) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$11,'open',$12) ON CONFLICT(repo_id,number) DO UPDATE SET version=$7,scope=$8,advisory=$9,vulnerability=$10,updated_at=$11,state=CASE WHEN dependabot_alerts.state='fixed' THEN 'open' ELSE dependabot_alerts.state END,fixed_at=NULL")
            .bind(repo).bind(number).bind(a["ghsa_id"].as_str().unwrap()).bind(&p.manifest).bind(&p.ecosystem).bind(&p.name).bind(&p.version).bind(&p.scope).bind(advisory).bind(vulnerability).bind(now).bind(dependabot_alerts::manifest_digest(&p.manifest)).execute(&mut **tx).await?;
    }
    Ok(())
}
async fn authorize(tx: &mut Transaction<'_, Postgres>, actor: i64, repo: i64) -> Result<()> {
    ensure!(
        super::discussions::lock_repository_mutation_authority(tx, actor, repo).await?,
        Refusal::Denied
    );
    let settings: Option<String> = sqlx::query_scalar(
        "SELECT security_and_analysis FROM repository_security_settings WHERE repo_id=$1",
    )
    .bind(repo)
    .fetch_optional(&mut **tx)
    .await?;
    let settings = settings
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()?;
    ensure!(
        !settings.is_some_and(|s| s["dependabot_alerts"]["status"] == "disabled"),
        Refusal::Missing
    );
    Ok(())
}
pub async fn alerts(pool: &PgPool, actor: i64, repo: i64) -> Result<Vec<Alert>> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, repo).await?;
    refresh(&mut tx, repo).await?;
    let rows =
        sqlx::query_as("SELECT * FROM dependabot_alerts WHERE repo_id=$1 ORDER BY number DESC")
            .bind(repo)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn update(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    number: i64,
    input: &Update,
) -> Result<Alert> {
    input.validate()?;
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, repo).await?;
    refresh(&mut tx, repo).await?;
    let previous: Alert =
        sqlx::query_as("SELECT * FROM dependabot_alerts WHERE repo_id=$1 AND number=$2")
            .bind(repo)
            .bind(number)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    ensure!(previous.state != "fixed", Refusal::Conflict);
    let now = Utc::now();
    let dismissed = input.state == "dismissed";
    let row = sqlx::query_as("UPDATE dependabot_alerts SET state=$3,dismissed_reason=$4,dismissed_comment=$5,dismissed_at=$6,dismissed_by=$7,updated_at=$8 WHERE repo_id=$1 AND number=$2 RETURNING *")
        .bind(repo).bind(number).bind(&input.state).bind(&input.dismissed_reason).bind(&input.dismissed_comment).bind(dismissed.then_some(now)).bind(dismissed.then_some(actor)).bind(now).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn organization_repositories(pool: &PgPool, actor: i64, org: i64) -> Result<Vec<i64>> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.role IN ('owner','admin') AND NOT u.is_banned AND NOT u.must_change_password AND EXISTS(SELECT 1 FROM org_memberships owners JOIN users active ON active.id=owners.user_id WHERE owners.org_id=m.org_id AND owners.role='owner' AND NOT active.is_banned AND NOT active.must_change_password))").bind(org).bind(actor).fetch_one(&mut *tx).await?;
    ensure!(allowed, Refusal::Denied);
    let rows = sqlx::query_scalar(
        "SELECT r.id FROM repositories r LEFT JOIN repository_security_settings s ON s.repo_id=r.id WHERE r.owner_kind='org' AND r.owner_org_id=$1 AND NOT r.is_archived AND COALESCE(s.security_and_analysis::jsonb #>> '{dependabot_alerts,status}', '') <> 'disabled' ORDER BY r.id",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}
