use crate::code_security::merge;
use crate::code_security::SettingsPage;
use crate::code_security::SETTINGS_PAGE_SIZE;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::queries::code_security::Configuration;
use codeyard_db_core::queries::code_security::Refusal;
use codeyard_db_core::queries::code_security::RepositorySettings;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

async fn write<'a>(
    pool: &'a SqlitePool,
    actor: &codeyard_db_core::queries::credentials::Actor,
    org: i64,
) -> Result<Transaction<'a, Sqlite>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    match codeyard_db_core::queries::sqlite::organizations::authority::authority(
        &mut tx,
        actor.user_id,
        org,
    )
    .await?
    {
        OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => {}
        OrganizationOutcome::Applied(OrgRole::Member)
            if codeyard_db_core::queries::sqlite::organizations::roles::manages_security(
                &mut tx,
                actor.user_id,
                org,
            )
            .await? => {}
        OrganizationOutcome::Missing => return Err(Refusal::Missing.into()),
        _ => return Err(Refusal::Denied.into()),
    }
    ensure!(
        codeyard_db_core::queries::sqlite::credentials::organization(
            &mut tx,
            actor,
            org,
            "organization_administration",
            true
        )
        .await?,
        Refusal::Denied
    );
    Ok(tx)
}

pub async fn list(pool: &SqlitePool, actor: i64, org: i64) -> Result<Vec<Configuration>> {
    let mut tx = pool.begin().await?;
    readable(&mut tx, actor, org).await?;
    let rows =
        sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1 ORDER BY id")
            .bind(org)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn settings_page(
    pool: &SqlitePool,
    actor: i64,
    session: &str,
    org: i64,
    before: Option<i64>,
) -> Result<SettingsPage> {
    let mut tx = pool.begin().await?;
    readable(&mut tx, actor, org).await?;
    ensure!(before.is_none_or(|id| id > 0), Refusal::Invalid);
    let (name, writable) = sqlx::query_as::<_, (String, bool)>(
        "SELECT o.name,EXISTS(SELECT 1 FROM org_memberships owner JOIN users active ON active.id=owner.user_id
         WHERE owner.org_id=o.id AND owner.role='owner' AND NOT active.is_banned AND NOT active.must_change_password)
         FROM organizations o JOIN sessions s ON s.user_id=?1 AND s.id=?2 JOIN users u ON u.id=s.user_id
         WHERE o.id=?3 AND julianday(s.expires_at)>julianday('now') AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(actor).bind(session).bind(org).fetch_optional(&mut *tx).await?.ok_or(Refusal::Denied)?;
    let rows = sqlx::query_as(
        "WITH page AS MATERIALIZED (
             SELECT id FROM code_security_configurations WHERE org_id=?1 AND id<=?2 ORDER BY id DESC LIMIT ?3
         ) SELECT c.* FROM page p JOIN code_security_configurations c ON c.id=p.id ORDER BY c.id DESC")
        .bind(org).bind(before.map(|id| id - 1).unwrap_or(i64::MAX)).bind((SETTINGS_PAGE_SIZE + 1) as i64)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(SettingsPage::from_rows(name, writable, rows))
}

pub async fn list_window(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    after: i64,
    before: i64,
    limit: i64,
) -> Result<Vec<Configuration>> {
    ensure!(after >= 0 && (1..=101).contains(&limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    readable(&mut tx, actor, org).await?;
    let direction = if before < i64::MAX { "DESC" } else { "ASC" };
    let rows = sqlx::query_as(&format!(
        "SELECT * FROM code_security_configurations WHERE org_id=$1 AND id>$2 AND id<$3 ORDER BY id {direction} LIMIT $4"
    ))
    .bind(org)
    .bind(after)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn save(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    id: Option<i64>,
    patch: &serde_json::Value,
) -> Result<Configuration> {
    let actor = actor.into();
    let mut tx = write(pool, &actor, org).await?;
    let actor_id = actor.user_id;
    let current = match id {
        Some(id) => Some(configuration(&mut tx, org, id).await?),
        None => None,
    };
    let settings = merge(current.as_ref().map(|c| c.settings.as_str()), patch)?;
    let name = settings["name"].as_str().ok_or(Refusal::Invalid)?;
    let duplicate:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM code_security_configurations WHERE org_id=$1 AND name=$2 AND id<>$3)").bind(org).bind(name).bind(id.unwrap_or(-1)).fetch_one(&mut *tx).await?;
    ensure!(!duplicate, Refusal::Duplicate);
    let now = chrono::Utc::now();
    let row: Configuration = if let Some(id) = id {
        sqlx::query_as("UPDATE code_security_configurations SET name=$3,settings=$4,updated_at=$5 WHERE org_id=$1 AND id=$2 RETURNING *").bind(org).bind(id).bind(name).bind(settings.to_string()).bind(now).fetch_one(&mut *tx).await?
    } else {
        sqlx::query_as("INSERT INTO code_security_configurations(org_id,name,settings,created_at,updated_at) VALUES($1,$2,$3,$4,$4) RETURNING *").bind(org).bind(name).bind(settings.to_string()).bind(now).fetch_one(&mut *tx).await?
    };
    let repos:Vec<i64>=sqlx::query_scalar("SELECT r.id FROM repositories r JOIN repository_security_settings s ON s.repo_id=r.id WHERE r.owner_org_id=$1 AND s.configuration_id=$2 ORDER BY r.id").bind(org).bind(row.id).fetch_all(&mut *tx).await?;
    for repo in repos {
        apply_repository(&mut tx, repo, Some(actor_id), Some(&row)).await?;
    }
    tx.commit().await?;
    Ok(row)
}
pub async fn delete(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    id: i64,
) -> Result<()> {
    let actor = actor.into();
    let mut tx = write(pool, &actor, org).await?;
    configuration(&mut tx, org, id).await?;
    sqlx::query("DELETE FROM code_security_configurations WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn set_default(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    id: i64,
    scope: &str,
) -> Result<Configuration> {
    ensure!(
        ["all", "none", "public", "private_and_internal"].contains(&scope),
        Refusal::Invalid
    );
    let actor = actor.into();
    let mut tx = write(pool, &actor, org).await?;
    configuration(&mut tx, org, id).await?;
    let rows: Vec<Configuration> =
        sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1")
            .bind(org)
            .fetch_all(&mut *tx)
            .await?;
    for row in rows {
        let next = if row.id == id {
            scope
        } else if scope == "all" || row.default_for_new_repos == scope {
            "none"
        } else if row.default_for_new_repos == "all" {
            match scope {
                "public" => "private_and_internal",
                "private_and_internal" => "public",
                _ => "all",
            }
        } else {
            &row.default_for_new_repos
        };
        sqlx::query("UPDATE code_security_configurations SET default_for_new_repos=$2 WHERE id=$1")
            .bind(row.id)
            .bind(next)
            .execute(&mut *tx)
            .await?;
    }
    let row = configuration(&mut tx, org, id).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn attach(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    id: Option<i64>,
    scope: &str,
    selected: &[i64],
) -> Result<()> {
    ensure!(
        [
            "all",
            "all_without_configurations",
            "public",
            "private_or_internal",
            "selected"
        ]
        .contains(&scope),
        Refusal::Invalid
    );
    ensure!(
        if scope == "selected" {
            !selected.is_empty() && selected.len() <= 250 && selected.iter().all(|id| *id > 0)
        } else {
            selected.is_empty()
        },
        Refusal::Invalid
    );
    let actor = actor.into();
    let mut tx = write(pool, &actor, org).await?;
    let actor_id = actor.user_id;
    let config = match id {
        Some(id) => Some(configuration(&mut tx, org, id).await?),
        None => None,
    };
    let rows: Vec<i64> = sqlx::query_scalar("SELECT r.id FROM repositories r WHERE r.owner_org_id=?1 AND (?2='all' OR (?2='selected' AND r.id IN (SELECT value FROM json_each(?3))) OR (?2='public' AND NOT r.is_private) OR (?2='private_or_internal' AND r.is_private) OR (?2='all_without_configurations' AND NOT EXISTS(SELECT 1 FROM repository_security_settings s WHERE s.repo_id=r.id AND s.configuration_id IS NOT NULL))) ORDER BY r.id")
        .bind(org).bind(scope).bind(serde_json::to_string(selected)?).fetch_all(&mut *tx).await?;
    ensure!(
        selected.iter().all(|id| rows.contains(id)),
        Refusal::Missing
    );
    for repo in rows {
        apply_repository(&mut tx, repo, Some(actor_id), config.as_ref()).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn repository_settings(
    pool: &SqlitePool,
    repo: i64,
) -> Result<Option<RepositorySettings>> {
    Ok(
        sqlx::query_as("SELECT * FROM repository_security_settings WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(pool)
            .await?,
    )
}

use codeyard_db_core::queries::sqlite::code_security::configuration;

use codeyard_db_core::queries::sqlite::code_security::apply_repository;

use codeyard_db_core::queries::sqlite::code_security::readable;

use codeyard_db_core::queries::sqlite::code_security::repositories;
