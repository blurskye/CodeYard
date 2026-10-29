use crate::backend::db::{
    action_settings::{policy_update, Policy, Refusal, Scope, Setting},
    organizations::{self, OrganizationOutcome},
};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    token: i64,
    scope: &Scope,
    write: bool,
) -> Result<(&'static str, &'static str, i64)> {
    match scope {
        Scope::Repository(repo) => {
            let allowed = if write {
                super::discussions::lock_repository_mutation_authority(tx, actor, *repo).await?
            } else {
                super::discussions::lock_policy_authority(tx, actor, *repo, false).await?
            };
            ensure!(allowed, Refusal::Denied);
            if write {
                ensure!(super::users::tokens::permits_write(tx, actor, token, Some(*repo), None, "administration").await?, Refusal::Denied);
            }
            Ok(("repo_actions_settings", "repo_id", *repo))
        }
        Scope::Organization(name) => {
            let org: i64 = sqlx::query_scalar("SELECT id FROM organizations WHERE name=$1")
                .bind(name)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(Refusal::Missing)?;
            let role = if write {
                super::organizations::authority::authority(tx, actor, org).await?
            } else {
                super::organizations::authority::read_authority(tx, actor, org).await?
            };
            ensure!(
                matches!(role, OrganizationOutcome::Applied(role) if organizations::administers(role)),
                Refusal::Denied
            );
            if write {
                ensure!(super::users::tokens::permits_write(tx, actor, token, None, Some(org), "organization_administration").await?, Refusal::Denied);
            }
            Ok(("org_actions_settings", "org_id", org))
        }
    }
}

pub(crate) async fn snapshot(connection: &mut PgConnection, repo: i64) -> Result<(Value, Value)> {
    let (own, org): (String, String) = sqlx::query_as("SELECT COALESCE(s.rest_settings,'{}'),COALESCE(o.rest_settings,'{}') FROM repositories r LEFT JOIN repo_actions_settings s ON s.repo_id=r.id LEFT JOIN org_actions_settings o ON o.org_id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=$1").bind(repo).fetch_optional(connection).await?.ok_or(Refusal::Missing)?;
    Ok((serde_json::from_str(&own)?, serde_json::from_str(&org)?))
}

/// Whether a fork pull request run may be admitted or claimed for this repository.
pub(crate) async fn fork_runs_allowed(connection: &mut PgConnection, repo: i64) -> Result<bool> {
    let private: Option<bool> = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=$1")
        .bind(repo)
        .fetch_optional(&mut *connection)
        .await?;
    if private == Some(false) {
        return Ok(true);
    }
    let (own, org) = snapshot(connection, repo).await?;
    Ok(crate::backend::db::action_settings::fork_workflows_allowed(&own, &org))
}

pub async fn effective(pool: &PgPool, repo: i64) -> Result<(Value, Value)> {
    snapshot(&mut *pool.acquire().await?, repo).await
}

pub async fn settings(
    pool: &PgPool,
    actor: i64,
    token: i64,
    scope: &Scope,
    update: Option<(Setting, &Value)>,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    let (table, column, id) = authority(&mut tx, actor, token, scope, update.is_some()).await?;
    let stored: Option<String> = sqlx::query_scalar(&format!(
        "SELECT rest_settings FROM {table} WHERE {column}=$1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let mut stored: Value = serde_json::from_str(stored.as_deref().unwrap_or("{}"))?;
    let site = super::actions::get_actions_settings(&mut *tx).await?;
    let mut maximum = i64::from(site.artifact_retention_days.unwrap_or(365));
    let mut parent = json!({});
    if let Scope::Repository(repo) = scope {
        let private: bool = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=$1")
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
    if let Some((setting, input)) = update {
        setting.update(&mut stored, input, maximum)?;
        sqlx::query(&format!("INSERT INTO {table}({column},rest_settings) VALUES($1,$2) ON CONFLICT({column}) DO UPDATE SET rest_settings=EXCLUDED.rest_settings,updated_at=NOW()")).bind(id).bind(stored.to_string()).execute(&mut *tx).await?;
    }
    let days = stored[Setting::Retention.key()]["days"]
        .as_i64()
        .unwrap_or(i64::from(site.artifact_retention_days.unwrap_or(90)))
        .min(maximum);
    stored[Setting::Retention.key()] = json!({"days":days,"maximum_allowed_days":maximum});
    let write = crate::backend::db::action_settings::workflow_write(
        &stored,
        &parent,
        site.default_workflow_permissions == "permissive",
    );
    if stored.get(Setting::Workflow.key()).is_none() {
        stored[Setting::Workflow.key()] = Setting::Workflow.defaults();
    }
    stored[Setting::Workflow.key()]["default_workflow_permissions"] =
        json!(if write { "write" } else { "read" });
    tx.commit().await?;
    Ok(stored)
}

pub async fn repositories(
    pool: &PgPool,
    actor: i64,
    token: i64,
    scope: &Scope,
    setting: Setting,
    change: Option<&crate::backend::db::action_settings::SelectionChange>,
    window: (i64, i64),
) -> Result<(i64, Vec<crate::backend::db::models::Repository>)> {
    let (offset, limit) = window;
    ensure!(matches!(scope, Scope::Organization(_)) && offset >= 0 && (1..=100).contains(&limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    if change.is_none() {
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    }
    let (_, _, org) = authority(&mut tx, actor, token, scope, change.is_some()).await?;
    let stored: Option<String> = sqlx::query_scalar("SELECT rest_settings FROM org_actions_settings WHERE org_id=$1")
        .bind(org).fetch_optional(&mut *tx).await?;
    let mut stored: Value = serde_json::from_str(stored.as_deref().unwrap_or("{}"))?;
    if let Some(change) = change {
        let mut added = crate::backend::db::action_settings::change_selection(&mut stored, setting, change)?;
        added.sort_unstable();
        added.dedup();
        let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repositories WHERE owner_org_id=$1 AND id=ANY($2)")
            .bind(org).bind(&added).fetch_one(&mut *tx).await?;
        ensure!(owned == added.len() as i64, Refusal::Invalid);
        sqlx::query("INSERT INTO org_actions_settings(org_id,rest_settings) VALUES($1,$2) ON CONFLICT(org_id) DO UPDATE SET rest_settings=EXCLUDED.rest_settings,updated_at=NOW()")
            .bind(org).bind(stored.to_string()).execute(&mut *tx).await?;
    }
    let ids: Vec<i64> = stored[setting.key()]["selected_repository_ids"].as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    let total = sqlx::query_scalar("SELECT COUNT(*) FROM repositories WHERE owner_org_id=$1 AND id=ANY($2)")
        .bind(org).bind(&ids).fetch_one(&mut *tx).await?;
    let rows = sqlx::query_as("SELECT * FROM repositories WHERE owner_org_id=$1 AND id=ANY($2) ORDER BY id LIMIT $3 OFFSET $4")
        .bind(org).bind(&ids).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((total, rows))
}

pub async fn policies_page(pool: &PgPool, actor: i64, token: i64, scope: &Scope, offset: i64, limit: i64) -> Result<(i64, Vec<Policy>)> {
    ensure!(offset >= 0 && (1..=100).contains(&limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").execute(&mut *tx).await?;
    let (_, column, scope_id) = authority(&mut tx, actor, token, scope, false).await?;
    let total = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM actions_policies WHERE {column}=$1")).bind(scope_id).fetch_one(&mut *tx).await?;
    let rows = sqlx::query_as(&format!("SELECT id,data,created_at,updated_at FROM actions_policies WHERE {column}=$1 ORDER BY id LIMIT $2 OFFSET $3")).bind(scope_id).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((total, rows))
}

pub async fn policies(
    pool: &PgPool,
    actor: i64,
    token: i64,
    scope: &Scope,
    id: Option<i64>,
    change: Option<Option<&Value>>,
) -> Result<Vec<Policy>> {
    let mut tx = pool.begin().await?;
    let (_, column, scope_id) = authority(&mut tx, actor, token, scope, change.is_some()).await?;
    ensure!(id.is_some() || change.is_some(), Refusal::Invalid);
    let mut rows: Vec<Policy> = if let Some(id) = id {
        let lock = if change.is_some() { "FOR UPDATE" } else { "" };
        sqlx::query_as(&format!("SELECT id,data,created_at,updated_at FROM actions_policies WHERE {column}=$1 AND id=$2 {lock}")).bind(scope_id).bind(id).fetch_all(&mut *tx).await?
    } else { Vec::new() };
    ensure!(id.is_none() || !rows.is_empty(), Refusal::Missing);
    if let Some(change) = change {
        if let Some(input) = change {
            let data = policy_update(
                id.and_then(|_| rows.first().map(|r| r.data.as_str())),
                input,
            )?;
            let row = if let Some(id) = id {
                sqlx::query_as("UPDATE actions_policies SET data=$2,updated_at=NOW() WHERE id=$1 RETURNING id,data,created_at,updated_at").bind(id).bind(data.to_string()).fetch_one(&mut *tx).await?
            } else {
                sqlx::query_as(&format!("INSERT INTO actions_policies({column},data) VALUES($1,$2) RETURNING id,data,created_at,updated_at")).bind(scope_id).bind(data.to_string()).fetch_one(&mut *tx).await?
            };
            rows = vec![row];
        } else {
            let id = id.ok_or(Refusal::Missing)?;
            sqlx::query("DELETE FROM actions_policies WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            rows.clear();
        }
    }
    tx.commit().await?;
    Ok(rows)
}
