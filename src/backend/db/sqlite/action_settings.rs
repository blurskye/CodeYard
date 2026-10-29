use crate::backend::db::{
    action_settings::{policy_update, Policy, PolicyPage, Refusal, Scope, Setting},
    credentials::Actor,
    organizations::{self, OrganizationOutcome},
};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &Actor,
    scope: &Scope,
    write: bool,
    organization_permission: &str,
) -> Result<(&'static str, &'static str, i64)> {
    match scope {
        Scope::Repository(repo) => {
            let allowed = if write {
                super::discussions::lock_admin_authority(tx, actor.user_id, *repo).await?
            } else {
                super::discussions::lock_policy_authority(tx, actor.user_id, *repo, false).await?
            };
            ensure!(allowed && super::credentials::repository(tx, actor, *repo, "administration", write).await?, Refusal::Denied);
            Ok(("repo_actions_settings", "repo_id", *repo))
        }
        Scope::Organization(name) => {
            let org: i64 = sqlx::query_scalar("SELECT id FROM organizations WHERE name=?1")
                .bind(name)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(Refusal::Missing)?;
            let role = if write {
                super::organizations::authority::authority(tx, actor.user_id, org).await?
            } else {
                super::organizations::authority::read_authority(tx, actor.user_id, org).await?
            };
            ensure!(
                matches!(role, OrganizationOutcome::Applied(role) if organizations::administers(role))
                    && super::credentials::organization(tx, actor, org, organization_permission, write).await?,
                Refusal::Denied
            );
            Ok(("org_actions_settings", "org_id", org))
        }
    }
}

pub(crate) async fn snapshot(
    connection: &mut SqliteConnection,
    repo: i64,
) -> Result<(Value, Value)> {
    let (own, org): (String, String) = sqlx::query_as("SELECT COALESCE(s.rest_settings,'{}'),COALESCE(o.rest_settings,'{}') FROM repositories r LEFT JOIN repo_actions_settings s ON s.repo_id=r.id LEFT JOIN org_actions_settings o ON o.org_id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=?1").bind(repo).fetch_optional(connection).await?.ok_or(Refusal::Missing)?;
    Ok((serde_json::from_str(&own)?, serde_json::from_str(&org)?))
}

/// Whether a fork pull request run may be admitted or claimed for this repository.
pub(crate) async fn fork_runs_allowed(connection: &mut SqliteConnection, repo: i64) -> Result<bool> {
    let private: Option<bool> = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=?1")
        .bind(repo)
        .fetch_optional(&mut *connection)
        .await?;
    if private == Some(false) {
        return Ok(true);
    }
    let (own, org) = snapshot(connection, repo).await?;
    Ok(crate::backend::db::action_settings::fork_workflows_allowed(&own, &org))
}

pub async fn effective(pool: &SqlitePool, repo: i64) -> Result<(Value, Value)> {
    snapshot(&mut *pool.acquire().await?, repo).await
}

pub async fn settings(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    scope: &Scope,
    setting: Setting,
    update: Option<&Value>,
) -> Result<Value> {
    let actor = actor.into();
    let mut tx = if update.is_some() { pool.begin_with("BEGIN IMMEDIATE").await? } else { pool.begin().await? };
    let (table, column, id) = authority(&mut tx, &actor, scope, update.is_some(), setting.organization_permission()).await?;
    let stored: Option<String> = sqlx::query_scalar(&format!(
        "SELECT rest_settings FROM {table} WHERE {column}=?1"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let mut stored: Value = serde_json::from_str(stored.as_deref().unwrap_or("{}"))?;
    let site = super::actions::get_actions_settings(&mut *tx).await?;
    let mut maximum = i64::from(site.artifact_retention_days.unwrap_or(365));
    let mut parent = json!({});
    let mut private = true;
    if let Scope::Repository(repo) = scope {
        private = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=?1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
        if !private {
            maximum = maximum.min(90);
        }
        parent = snapshot(&mut tx, *repo).await?.1;
        if let Some(days) = parent[Setting::Retention.key()]["days"].as_i64() {
            maximum = maximum.min(days);
        }
    }
    if let Some(input) = update {
        setting.update(&mut stored, input, maximum)?;
        sqlx::query(&format!("INSERT INTO {table}({column},rest_settings) VALUES(?1,?2) ON CONFLICT({column}) DO UPDATE SET rest_settings=EXCLUDED.rest_settings,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')")).bind(id).bind(stored.to_string()).execute(&mut *tx).await?;
    }
    let stored = crate::backend::db::action_settings::resolved(stored, &parent, &site, private, maximum);
    tx.commit().await?;
    Ok(stored)
}

pub async fn policies(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    scope: &Scope,
    id: Option<i64>,
    change: Option<Option<&Value>>,
    offset: i64,
    limit: i64,
) -> Result<PolicyPage> {
    ensure!(offset >= 0 && (1..=100).contains(&limit) && (id.is_none() && change.is_none() || offset == 0), Refusal::Invalid);
    let actor = actor.into();
    let mut tx = if change.is_some() { pool.begin_with("BEGIN IMMEDIATE").await? } else { pool.begin().await? };
    let (_, column, scope_id) = authority(&mut tx, &actor, scope, change.is_some(), "organization_administration").await?;
    let total: i64 = if change.is_none() {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM actions_policies WHERE {column}=?1 AND (?2 IS NULL OR id=?2)"))
            .bind(scope_id).bind(id).fetch_one(&mut *tx).await?
    } else { 0 };
    let mut rows: Vec<Policy> = if change.is_some() && id.is_none() { Vec::new() } else {
        sqlx::query_as(&format!("SELECT id,data,created_at,updated_at FROM actions_policies WHERE {column}=?1 AND (?2 IS NULL OR id=?2) ORDER BY id LIMIT ?3 OFFSET ?4"))
            .bind(scope_id).bind(id).bind(limit).bind(offset).fetch_all(&mut *tx).await?
    };
    ensure!(id.is_none() || !rows.is_empty(), Refusal::Missing);
    if let Some(change) = change {
        if let Some(input) = change {
            let data = policy_update(id.and_then(|_| rows.first().map(|r| r.data.as_str())), input)?;
            let row = if let Some(id) = id {
                sqlx::query_as("UPDATE actions_policies SET data=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 RETURNING id,data,created_at,updated_at").bind(id).bind(data.to_string()).fetch_one(&mut *tx).await?
            } else {
                sqlx::query_as(&format!("INSERT INTO actions_policies({column},data) VALUES(?1,?2) RETURNING id,data,created_at,updated_at")).bind(scope_id).bind(data.to_string()).fetch_one(&mut *tx).await?
            };
            rows = vec![row];
        } else {
            let id = id.ok_or(Refusal::Missing)?;
            sqlx::query("DELETE FROM actions_policies WHERE id=?1").bind(id).execute(&mut *tx).await?;
            rows.clear();
        }
    }
    let total = if change.is_some() { rows.len() as i64 } else { total };
    tx.commit().await?;
    Ok(PolicyPage { total, rows })
}
