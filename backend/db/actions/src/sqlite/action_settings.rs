use crate::action_settings::policy_update;
use crate::action_settings::ListedPolicy;
use crate::action_settings::Policy;
use crate::action_settings::PolicySelector;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::pool::SqlitePool;
use codeyard_db_core::queries::action_settings::Refusal;
use codeyard_db_core::queries::action_settings::Scope;
use codeyard_db_core::queries::action_settings::Setting;
use codeyard_db_core::queries::credentials::Actor;
use serde_json::json;
use serde_json::Value;

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
    let mut tx = if update.is_some() {
        pool.begin_with("BEGIN IMMEDIATE").await?
    } else {
        pool.begin().await?
    };
    let (table, column, id) = authority(
        &mut tx,
        &actor,
        scope,
        update.is_some(),
        setting.organization_permission(),
    )
    .await?;
    let stored: Option<String> = sqlx::query_scalar(&format!(
        "SELECT rest_settings FROM {table} WHERE {column}=?1"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let mut stored: Value = serde_json::from_str(stored.as_deref().unwrap_or("{}"))?;
    let site = crate::sqlite::actions::get_actions_settings(&mut *tx).await?;
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
    let stored = crate::action_settings::resolved(stored, &parent, &site, private, maximum);
    tx.commit().await?;
    Ok(stored)
}

pub async fn policies_page(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    scope: &Scope,
    has_parents: bool,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<ListedPolicy>)> {
    ensure!(offset >= 0 && (1..=100).contains(&limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    let actor = actor.into();
    let (_, column, scope_id) =
        authority(&mut tx, &actor, scope, false, "organization_administration").await?;
    let repository = if let Scope::Repository(repo) = scope {
        if has_parents {
            sqlx::query_as::<_, codeyard_db_core::models::Repository>("SELECT r.* FROM repositories r WHERE r.id=?1 AND r.owner_kind='org' AND EXISTS(SELECT 1 FROM actions_policies p WHERE p.org_id=r.owner_org_id)").bind(repo).fetch_optional(&mut *tx).await?
        } else {
            None
        }
    } else {
        None
    };
    let selected = if let Some(repository) = repository
        .as_ref()
        .filter(|repo| repo.owner_kind == "org" && repo.owner_org_id.is_some())
    {
        let org = repository.owner_org_id.ok_or(Refusal::Missing)?;
        let definitions =
            codeyard_db_orgs::sqlite::organizations::properties::definitions_on(&mut tx, org)
                .await?;
        let values = codeyard_db_core::queries::sqlite::organizations::properties::values_on(
            &mut tx,
            repository.id,
            &definitions,
        )
        .await?;
        let properties = values.into_iter().filter_map(|value| value.value.map(|stored| (value.property_name, match stored { codeyard_db_core::queries::organizations::properties::PropertyValue::String(value) => vec![value], codeyard_db_core::queries::organizations::properties::PropertyValue::Strings(values) => values }))).collect();
        let context = codeyard_db_core::queries::rulesets::matching::RepositoryContext {
            id: repository.id,
            name: &repository.name,
            default_branch: &repository.default_branch,
            is_private: repository.is_private,
            is_archived: repository.is_archived,
            is_fork: repository.is_fork,
            properties: &properties,
        };
        let mut budget = codeyard_model::workflow_glob::WorkflowMatchBudget::new();
        let (mut after, mut total, mut ids) = (0_i64, 0_i64, Vec::new());
        loop {
            let selectors: Vec<PolicySelector> = sqlx::query_as(&format!("SELECT p.id,COALESCE(p.{column}=?1,FALSE) AS own,CASE WHEN p.{column}=?1 THEN NULL ELSE json_extract(p.data,'$.conditions') END AS conditions FROM actions_policies p WHERE (p.{column}=?1 OR p.org_id=?2) AND p.id>?3 ORDER BY p.id LIMIT 100")).bind(scope_id).bind(org).bind(after).fetch_all(&mut *tx).await?;
            let finished = selectors.len() < 100;
            for selector in selectors {
                after = selector.id;
                if selector.applies(&context, &mut budget)? {
                    if total >= offset && ids.len() < limit as usize {
                        ids.push(selector.id);
                    }
                    total += 1;
                }
            }
            if finished {
                break;
            }
        }
        Some((total, ids))
    } else {
        None
    };
    let select = "SELECT p.id,COALESCE(p.repo_id,p.org_id) AS owner_id,p.data,p.created_at,p.updated_at,o.name AS organization FROM actions_policies p LEFT JOIN organizations o ON o.id=p.org_id";
    let (total, rows) = if let Some((total, ids)) = selected {
        let rows = sqlx::query_as(&format!(
            "{select} WHERE p.id IN (SELECT value FROM json_each(?1)) ORDER BY p.id"
        ))
        .bind(serde_json::to_string(&ids)?)
        .fetch_all(&mut *tx)
        .await?;
        (total, rows)
    } else {
        let total = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM actions_policies WHERE {column}=?1"
        ))
        .bind(scope_id)
        .fetch_one(&mut *tx)
        .await?;
        let rows = sqlx::query_as(&format!(
            "{select} WHERE p.{column}=?1 ORDER BY p.id LIMIT ?2 OFFSET ?3"
        ))
        .bind(scope_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await?;
        (total, rows)
    };
    tx.commit().await?;
    Ok((total, rows))
}

pub async fn policies(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    scope: &Scope,
    id: Option<i64>,
    change: Option<Option<&Value>>,
) -> Result<Vec<Policy>> {
    let actor = actor.into();
    let mut tx = if change.is_some() {
        pool.begin_with("BEGIN IMMEDIATE").await?
    } else {
        pool.begin().await?
    };
    let (_, column, scope_id) = authority(
        &mut tx,
        &actor,
        scope,
        change.is_some(),
        "organization_administration",
    )
    .await?;
    ensure!(id.is_some() || change.is_some(), Refusal::Invalid);
    let mut rows: Vec<Policy> = if let Some(id) = id {
        sqlx::query_as(&format!("SELECT id,COALESCE(repo_id,org_id) AS owner_id,data,created_at,updated_at FROM actions_policies WHERE {column}=?1 AND id=?2")).bind(scope_id).bind(id).fetch_all(&mut *tx).await?
    } else {
        Vec::new()
    };
    ensure!(id.is_none() || !rows.is_empty(), Refusal::Missing);
    if let Some(change) = change {
        if let Some(input) = change {
            let data = policy_update(
                id.and_then(|_| rows.first().map(|r| r.data.as_str())),
                input,
            )?;
            let row = if let Some(id) = id {
                sqlx::query_as("UPDATE actions_policies SET data=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 RETURNING id,COALESCE(repo_id,org_id) AS owner_id,data,created_at,updated_at").bind(id).bind(data.to_string()).fetch_one(&mut *tx).await?
            } else {
                sqlx::query_as(&format!("INSERT INTO actions_policies({column},data) VALUES(?1,?2) RETURNING id,COALESCE(repo_id,org_id) AS owner_id,data,created_at,updated_at")).bind(scope_id).bind(data.to_string()).fetch_one(&mut *tx).await?
            };
            rows = vec![row];
        } else {
            let id = id.ok_or(Refusal::Missing)?;
            sqlx::query("DELETE FROM actions_policies WHERE id=?1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            rows.clear();
        }
    }
    tx.commit().await?;
    Ok(rows)
}

use codeyard_db_core::queries::sqlite::action_settings::authority;

use codeyard_db_core::queries::sqlite::action_settings::snapshot;
