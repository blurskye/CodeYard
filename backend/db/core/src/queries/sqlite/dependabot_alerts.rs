use crate::queries::dependabot_alerts::Alert;
use anyhow::Result;
use chrono::Utc;
use codeyard_model::advisories::Advisory;
use serde_json::Value;
use sqlx::Sqlite;
use sqlx::Transaction;
/// The catalogue an inventory can match: advisories without an OSV record, and those whose
/// OSV record names one of the inventory's packages (`keys`, from `osv_keys`). Without keys it
/// is the whole catalogue. An imported document wins over a published one with the same GHSA ID.
pub async fn global_in(tx: &mut Transaction<'_, Sqlite>, keys: Option<&str>) -> Result<Vec<Value>> {
    let imported: Vec<String> = sqlx::query_scalar(
        "SELECT document FROM global_advisories WHERE $1 IS NULL OR osv_id IS NULL OR osv_id IN (SELECT p.advisory FROM osv_packages p JOIN json_each($1) j ON p.ecosystem=json_extract(j.value,'$[0]') AND p.name=json_extract(j.value,'$[1]')) ORDER BY ghsa_id",
    )
    .bind(keys)
    .fetch_all(&mut **tx)
    .await?;
    let mut documents = std::collections::BTreeMap::new();
    let published: Vec<Advisory> = sqlx::query_as("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id WHERE a.state='published' AND NOT r.is_private AND NOT EXISTS (SELECT 1 FROM global_advisories g WHERE g.ghsa_id=a.ghsa_id) ORDER BY a.ghsa_id").fetch_all(&mut **tx).await?;
    for row in published {
        match crate::queries::dependabot_alerts::published(row) {
            Ok(value) => {
                documents.insert(value["ghsa_id"].as_str().unwrap().to_owned(), value);
            }
            Err(_) => tracing::warn!("Invalid published advisory excluded from catalogue"),
        }
    }
    for body in imported {
        let value: Value = serde_json::from_str(&body)?;
        documents.insert(value["ghsa_id"].as_str().unwrap().to_owned(), value);
    }
    Ok(documents.into_values().collect())
}

pub async fn refresh(
    tx: &mut Transaction<'_, Sqlite>,
    repo: i64,
    pinned_sha: Option<&str>,
) -> Result<()> {
    let (branch, archived): (String, bool) =
        sqlx::query_as("SELECT default_branch,is_archived FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_one(&mut **tx)
            .await?;
    if archived || !crate::queries::sqlite::dependency_graph::enabled_in(tx, repo).await? {
        return Ok(());
    }
    let reference = format!("refs/heads/{branch}");
    let sha: Option<String> = sqlx::query_scalar(&format!(
        "SELECT {}",
        crate::queries::dependency_graph::DEFAULT_SNAPSHOT_SHA_SQL
    ))
    .bind(repo)
    .fetch_one(&mut **tx)
    .await?;
    if let (Some(pinned), Some(current)) = (pinned_sha, sha.as_ref()) {
        if !pinned.eq_ignore_ascii_case(current) {
            return Ok(());
        }
    }
    // No inventory is not evidence of a fix. All contributing detectors share one revision.
    let Some(sha) = sha else { return Ok(()) };
    let snapshots = crate::queries::sqlite::dependency_graph::snapshots_in(
        tx,
        repo,
        Some(&reference),
        Some(&sha),
    )
    .await?;
    for snapshot in &snapshots {
        snapshot.validate()?;
    }
    let keys = crate::queries::dependabot_alerts::osv_keys(&snapshots);
    let mut advisories = global_in(tx, Some(&keys)).await?;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT a.data FROM osv_advisories a WHERE a.id IN (SELECT p.advisory FROM osv_packages p JOIN json_each($1) j ON p.ecosystem=json_extract(j.value,'$[0]') AND p.name=json_extract(j.value,'$[1]')) ORDER BY a.id",
    )
    .bind(&keys)
    .fetch_all(&mut **tx)
    .await?;
    let (mut osv, aliases) = crate::queries::dependabot_alerts::osv_records(rows)?;
    let overrides: Vec<(String, Option<String>)> = sqlx::query_as("SELECT document,osv_id FROM global_advisories WHERE ghsa_id IN (SELECT value FROM json_each($1))")
        .bind(serde_json::to_string(&aliases)?).fetch_all(&mut **tx).await?;
    crate::queries::dependabot_alerts::catalogue_for_records(&mut advisories, &mut osv, overrides)?;
    let matches = crate::queries::dependabot_alerts::matches(&snapshots, &advisories, &osv)?;
    let now = Utc::now();
    let previous: Vec<Alert> =
        sqlx::query_as("SELECT * FROM dependabot_alerts WHERE repo_id=$1 ORDER BY number")
            .bind(repo)
            .fetch_all(&mut **tx)
            .await?;
    let matched: std::collections::HashSet<_> = matches
        .iter()
        .map(|(p, a, _)| {
            (
                a["ghsa_id"].as_str().unwrap(),
                p.manifest.as_str(),
                p.ecosystem.as_str(),
                p.name.as_str(),
            )
        })
        .collect();
    let known: std::collections::HashMap<_, _> = previous
        .iter()
        .map(|row| {
            (
                (
                    row.ghsa_id.as_str(),
                    row.manifest.as_str(),
                    row.ecosystem.as_str(),
                    row.package.as_str(),
                ),
                row,
            )
        })
        .collect();
    for row in &previous {
        let found = matched.contains(&(
            row.ghsa_id.as_str(),
            row.manifest.as_str(),
            row.ecosystem.as_str(),
            row.package.as_str(),
        ));
        if !found && row.state == "open" {
            sqlx::query("UPDATE dependabot_alerts SET state='fixed',fixed_at=$3,updated_at=$3 WHERE repo_id=$1 AND number=$2").bind(repo).bind(row.number).bind(now).execute(&mut **tx).await?;
        }
    }
    drop(matched);
    let mut next = previous.last().map_or(1, |r| r.number + 1);
    for (p, a, v) in matches {
        let old = known
            .get(&(
                a["ghsa_id"].as_str().unwrap(),
                p.manifest.as_str(),
                p.ecosystem.as_str(),
                p.name.as_str(),
            ))
            .copied();
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
            .bind(repo).bind(number).bind(a["ghsa_id"].as_str().unwrap()).bind(&p.manifest).bind(&p.ecosystem).bind(&p.name).bind(&p.version).bind(&p.scope).bind(advisory).bind(vulnerability).bind(now).bind(crate::queries::dependabot_alerts::manifest_digest(&p.manifest)).execute(&mut **tx).await?;
    }
    Ok(())
}
