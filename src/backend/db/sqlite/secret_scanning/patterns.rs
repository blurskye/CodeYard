use crate::backend::db::secret_scanning::{ConfigUpdate, PatternConfig, Refusal};
use anyhow::{ensure, Result};
use codeyard_git::secret_detection::{
    patterns::{Create, Delete, Owner, Pattern, Update},
    secret_types,
};
use serde_json::{json, Value};
use sqlx::{Sqlite, SqlitePool, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    owner: Owner,
    write: bool,
) -> Result<()> {
    match owner {
        Owner::Organization(org) => super::org_authority(tx, actor, org).await,
        Owner::Repository(repo) => {
            let allowed = if write {
                super::super::discussions::lock_admin_authority(tx, actor, repo).await?
            } else {
                super::super::discussions::lock_archive_authority(tx, actor, repo).await?
            };
            ensure!(allowed, Refusal::Denied);
            Ok(())
        }
    }
}
async fn read(tx: &mut Transaction<'_, Sqlite>, owner: Owner) -> Result<Vec<Pattern>> {
    let (repo, org) = owner.ids();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT document FROM secret_scanning_patterns WHERE repo_id=$1 OR org_id=$2 ORDER BY id",
    )
    .bind(repo)
    .bind(org)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|row| Ok(serde_json::from_str(&row)?))
        .collect()
}
async fn read_one(tx: &mut Transaction<'_, Sqlite>, owner: Owner, id: i64) -> Result<Pattern> {
    let (repo, org) = owner.ids();
    let document: String = sqlx::query_scalar(
        "SELECT document FROM secret_scanning_patterns WHERE id=$1 AND (repo_id=$2 OR org_id=$3)",
    )
    .bind(id)
    .bind(repo)
    .bind(org)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Refusal::Missing)?;
    Ok(serde_json::from_str(&document)?)
}
async fn save(tx: &mut Transaction<'_, Sqlite>, pattern: &Pattern) -> Result<()> {
    sqlx::query("UPDATE secret_scanning_patterns SET document=$2 WHERE id=$1")
        .bind(pattern.id)
        .bind(serde_json::to_string(pattern)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn rescan(
    tx: &mut Transaction<'_, Sqlite>,
    owner: Owner,
    actor: i64,
    pattern: &Pattern,
    kind: &str,
) -> Result<()> {
    let (repo, org) = owner.ids();
    sqlx::query("DELETE FROM secret_scanning_commits WHERE repo_id IN (SELECT id FROM repositories WHERE id=$1 OR owner_org_id=$2)").bind(repo).bind(org).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO secret_scanning_jobs(repo_id,scan_type,actor_id,pattern_name,pattern_scope) SELECT s.repo_id,$4,$3,$5,$6 FROM secret_scanning_settings s JOIN repositories r ON r.id=s.repo_id WHERE s.enabled AND (r.id=$1 OR r.owner_org_id=$2)").bind(repo).bind(org).bind(actor).bind(kind).bind(&pattern.name).bind(match owner {Owner::Repository(_)=>"repository",Owner::Organization(_)=>"organization"}).execute(&mut **tx).await?;
    Ok(())
}
pub async fn list(pool: &SqlitePool, actor: i64, owner: Owner) -> Result<Vec<Pattern>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, actor, owner, false).await?;
    let rows = read(&mut tx, owner).await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn create(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    owner: Owner,
    input: &[Create],
) -> Result<Vec<Pattern>> {
    ensure!(!input.is_empty() && input.len() <= 100, Refusal::Invalid);
    let mut admission = pool.begin().await?;
    authority(&mut admission, actor, owner, true).await?;
    ensure!(
        super::super::users::transitions::credential_live(&mut admission, actor, credential, true)
            .await?,
        Refusal::Denied
    );
    admission.commit().await?;
    codeyard_git::secret_detection::patterns::validate_batch(input)
        .await
        .map_err(|_| Refusal::Invalid)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, actor, owner, true).await?;
    ensure!(
        super::super::users::transitions::credential_live(&mut tx, actor, credential, true).await?,
        Refusal::Denied
    );
    let (repo, org) = owner.ids();
    let previous: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM secret_scanning_patterns WHERE repo_id=$1 OR org_id=$2",
    )
    .bind(repo)
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    ensure!(previous.len() + input.len() <= 500, Refusal::Invalid);
    let mut names = previous
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let mut patterns = Vec::new();
    for item in input {
        ensure!(names.insert(item.name.clone()), Refusal::Invalid);
        let id:i64=sqlx::query_scalar("INSERT INTO secret_scanning_patterns(repo_id,org_id,name,document) VALUES($1,$2,$3,'') RETURNING id").bind(repo).bind(org).bind(&item.name).fetch_one(&mut *tx).await?;
        let pattern = item.clone().into_pattern(id);
        save(&mut tx, &pattern).await?;
        patterns.push(pattern);
    }
    tx.commit().await?;
    Ok(patterns)
}
pub async fn update(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    owner: Owner,
    id: i64,
    input: &Update,
) -> Result<Pattern> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, actor, owner, true).await?;
    ensure!(
        super::super::users::transitions::credential_live(&mut tx, actor, credential, true).await?,
        Refusal::Denied
    );
    let mut pattern = read_one(&mut tx, owner, id).await?;
    ensure!(
        input.custom_pattern_version.is_null() || input.custom_pattern_version.is_string(),
        Refusal::Invalid
    );
    ensure!(
        input.custom_pattern_version.as_str() == pattern.custom_pattern_version.as_deref(),
        Refusal::Version
    );
    pattern.apply(input).map_err(|_| Refusal::Invalid)?;
    save(&mut tx, &pattern).await?;
    if pattern.state == "published" {
        rescan(&mut tx, owner, actor, &pattern, "pattern_update").await?;
    }
    tx.commit().await?;
    Ok(pattern)
}
pub async fn publish(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    owner: Owner,
    id: i64,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, actor, owner, true).await?;
    ensure!(
        super::super::users::transitions::credential_live(&mut tx, actor, credential, true).await?,
        Refusal::Denied
    );
    let mut pattern = read_one(&mut tx, owner, id).await?;
    if pattern.state == "published" {
        tx.commit().await?;
        return Ok(());
    }
    pattern.state = "published".into();
    pattern.push_protection_enabled = true;
    if let Owner::Organization(org) = owner {
        let mut settings = config(&mut tx, org).await?;
        settings
            .settings
            .insert(format!("cp_{id}"), "enabled".into());
        let version = uuid::Uuid::new_v4().to_string();
        settings.version = Some(version.clone());
        sqlx::query("INSERT INTO secret_scanning_pattern_configs(org_id,version,document) VALUES($1,$2,$3) ON CONFLICT(org_id) DO UPDATE SET version=$2,document=$3").bind(org).bind(version).bind(serde_json::to_string(&settings)?).execute(&mut *tx).await?;
    }
    pattern.updated_at = chrono::Utc::now();
    save(&mut tx, &pattern).await?;
    rescan(&mut tx, owner, actor, &pattern, "custom_pattern_backfill").await?;
    tx.commit().await?;
    Ok(())
}
pub async fn delete(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    owner: Owner,
    input: &[Delete],
    resolve: bool,
) -> Result<()> {
    ensure!(!input.is_empty() && input.len() <= 500, Refusal::Invalid);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, actor, owner, true).await?;
    ensure!(
        super::super::users::transitions::credential_live(&mut tx, actor, credential, true).await?,
        Refusal::Denied
    );
    let patterns = read(&mut tx, owner).await?;
    let (repo, org) = owner.ids();
    for item in input {
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == item.pattern_id)
            .ok_or(Refusal::Missing)?;
        ensure!(
            item.custom_pattern_version
                .as_deref()
                .is_none_or(|version| Some(version) == pattern.custom_pattern_version.as_deref()),
            Refusal::Version
        );
        if resolve {
            sqlx::query("UPDATE secret_scanning_alerts SET state='resolved',resolution='pattern_deleted',resolution_comment='Custom pattern deleted',resolved_by=$4,resolved_at=$5,updated_at=$5 WHERE state='open' AND secret_type=$3 AND repo_id IN (SELECT id FROM repositories WHERE id=$1 OR owner_org_id=$2)").bind(repo).bind(org).bind(&pattern.slug).bind(actor).bind(chrono::Utc::now()).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM secret_scanning_alerts WHERE secret_type=$3 AND repo_id IN (SELECT id FROM repositories WHERE id=$1 OR owner_org_id=$2)").bind(repo).bind(org).bind(&pattern.slug).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM secret_scanning_patterns WHERE id=$1")
            .bind(pattern.id)
            .execute(&mut *tx)
            .await?;
    }
    if let Owner::Organization(org) = owner {
        let mut settings = config(&mut tx, org).await?;
        for item in input {
            settings.settings.remove(&format!("cp_{}", item.pattern_id));
        }
        let version = uuid::Uuid::new_v4().to_string();
        settings.version = Some(version.clone());
        sqlx::query("INSERT INTO secret_scanning_pattern_configs(org_id,version,document) VALUES($1,$2,$3) ON CONFLICT(org_id) DO UPDATE SET version=$2,document=$3")
            .bind(org).bind(version).bind(serde_json::to_string(&settings)?).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
async fn config(tx: &mut Transaction<'_, Sqlite>, org: i64) -> Result<PatternConfig> {
    let row: Option<String> =
        sqlx::query_scalar("SELECT document FROM secret_scanning_pattern_configs WHERE org_id=$1")
            .bind(org)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row
        .map(|row| serde_json::from_str(&row))
        .transpose()?
        .unwrap_or_default())
}
pub async fn effective(pool: &SqlitePool, repo: i64) -> Result<(Vec<Pattern>, Vec<String>)> {
    let mut tx = pool.begin().await?;
    let org: Option<i64> = sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=$1")
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT document FROM secret_scanning_patterns WHERE repo_id=$1 OR org_id=$2 ORDER BY id",
    )
    .bind(repo)
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    let mut patterns = rows
        .into_iter()
        .map(|row| serde_json::from_str::<Pattern>(&row))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let config = match org {
        Some(org) => config(&mut tx, org).await?,
        None => PatternConfig::default(),
    };
    for pattern in &mut patterns {
        if let Some(setting) = config.settings.get(&format!("cp_{}", pattern.id)) {
            pattern.push_protection_enabled = setting == "enabled";
        }
    }
    let disabled = config
        .settings
        .into_iter()
        .filter_map(|(kind, setting)| {
            (setting == "disabled" && !kind.starts_with("cp_")).then_some(kind.to_ascii_lowercase())
        })
        .collect();
    tx.commit().await?;
    Ok((patterns, disabled))
}
pub async fn configure(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    org: i64,
    input: &ConfigUpdate,
) -> Result<String> {
    ensure!(
        input.provider_pattern_settings.len() + input.custom_pattern_settings.len() <= 500,
        Refusal::Invalid
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, actor, Owner::Organization(org), true).await?;
    ensure!(
        super::super::users::transitions::credential_live(&mut tx, actor, credential, true).await?,
        Refusal::Denied
    );
    let mut config = config(&mut tx, org).await?;
    ensure!(
        config.version == input.pattern_config_version,
        Refusal::Conflict
    );
    let supported = secret_types()?;
    let mut patterns = read(&mut tx, Owner::Organization(org)).await?;
    for setting in &input.provider_pattern_settings {
        ensure!(
            matches!(
                setting.push_protection_setting.as_str(),
                "not-set" | "enabled" | "disabled"
            ) && supported
                .iter()
                .any(|(kind, _)| *kind == setting.token_type.to_ascii_lowercase()),
            Refusal::Invalid
        );
        config.settings.insert(
            setting.token_type.to_ascii_uppercase(),
            setting.push_protection_setting.clone(),
        );
    }
    for setting in &input.custom_pattern_settings {
        let pattern = patterns
            .iter_mut()
            .find(|pattern| format!("cp_{}", pattern.id) == setting.token_type)
            .ok_or(Refusal::Invalid)?;
        ensure!(
            pattern.custom_pattern_version == setting.custom_pattern_version,
            Refusal::Invalid
        );
        ensure!(
            matches!(
                setting.push_protection_setting.as_str(),
                "enabled" | "disabled"
            ) && pattern.state == "published",
            Refusal::Invalid
        );
        pattern.push_protection_enabled = setting.push_protection_setting == "enabled";
        save(&mut tx, pattern).await?;
        config.settings.insert(
            setting.token_type.clone(),
            setting.push_protection_setting.clone(),
        );
    }
    let version = uuid::Uuid::new_v4().to_string();
    config.version = Some(version.clone());
    sqlx::query("INSERT INTO secret_scanning_pattern_configs(org_id,version,document) VALUES($1,$2,$3) ON CONFLICT(org_id) DO UPDATE SET version=$2,document=$3").bind(org).bind(&version).bind(serde_json::to_string(&config)?).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(version)
}
pub async fn configs(pool: &SqlitePool, actor: i64, org: i64) -> Result<Value> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authority(&mut tx, actor, Owner::Organization(org), false).await?;
    let config = config(&mut tx, org).await?;
    let counts:Vec<(String,i64,i64)>=sqlx::query_as("SELECT secret_type,COUNT(*),SUM(CASE WHEN resolution='false_positive' THEN 1 ELSE 0 END) FROM secret_scanning_alerts WHERE repo_id IN (SELECT id FROM repositories WHERE owner_org_id=$1) GROUP BY secret_type").bind(org).fetch_all(&mut *tx).await?;
    let bypasses:Vec<(String,i64,i64)>=sqlx::query_as("SELECT token_type,COUNT(DISTINCT secret_hash),COUNT(DISTINCT CASE WHEN used_at IS NOT NULL THEN secret_hash END) FROM secret_scanning_bypasses WHERE repo_id IN (SELECT id FROM repositories WHERE owner_org_id=$1) GROUP BY token_type").bind(org).fetch_all(&mut *tx).await?;
    let total: i64 = counts.iter().map(|(_, count, _)| count).sum();
    let stats = |token: String,
                 slug: String,
                 name: String,
                 version: Option<String>,
                 default: &str| {
        let (count, false_positive) = counts
            .iter()
            .find(|(kind, _, _)| *kind == slug)
            .map(|(_, count, false_positive)| (*count, *false_positive))
            .unwrap_or_default();
        let (blocks, bypassed) = bypasses
            .iter()
            .find(|(kind, _, _)| *kind == slug)
            .map(|(_, count, used)| (*count, *used))
            .unwrap_or_default();
        let percent = |part: i64, all: i64| {
            if all == 0 {
                0
            } else {
                ((100 * part + all / 2) / all).min(100)
            }
        };
        json!({"token_type":token,"slug":slug,"display_name":name,"custom_pattern_version":version,"default_setting":default,"setting":config.settings.get(&token).map(String::as_str).unwrap_or("not-set"),"enterprise_setting":null,"alert_total":count,"alert_total_percentage":percent(count,total),"false_positives":false_positive,"false_positive_rate":percent(false_positive,count),"bypass_rate":percent(bypassed,blocks)})
    };
    let provider = secret_types()?
        .into_iter()
        .map(|(kind, name)| stats(kind.to_ascii_uppercase(), kind, name, None, "enabled"))
        .collect::<Vec<_>>();
    let custom = read(&mut tx, Owner::Organization(org))
        .await?
        .into_iter()
        .map(|pattern| {
            stats(
                format!("cp_{}", pattern.id),
                pattern.slug,
                pattern.name,
                pattern.custom_pattern_version,
                "disabled",
            )
        })
        .collect::<Vec<_>>();
    tx.commit().await?;
    Ok(
        json!({"pattern_config_version":config.version,"provider_pattern_overrides":provider,"custom_pattern_overrides":custom}),
    )
}
