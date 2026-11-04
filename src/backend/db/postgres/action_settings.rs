use crate::backend::db::{
    action_settings::{
        policy_update, ListedPolicy, Policy, PolicySelector, Refusal, Scope, Setting,
    },
    credentials::Actor,
    organizations::{self, OrganizationOutcome},
};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    actor: &Actor,
    scope: &Scope,
    write: bool,
    organization_permission: &str,
) -> Result<(&'static str, &'static str, i64)> {
    match scope {
        Scope::Repository(repo) => {
            let allowed = if write {
                super::discussions::lock_repository_mutation_authority(tx, actor.user_id, *repo)
                    .await?
            } else {
                super::discussions::lock_policy_authority(tx, actor.user_id, *repo, false).await?
            };
            ensure!(
                allowed
                    && super::credentials::repository(tx, actor, *repo, "administration", write)
                        .await?,
                Refusal::Denied
            );
            Ok(("repo_actions_settings", "repo_id", *repo))
        }
        Scope::Organization(name) => {
            let org: i64 = sqlx::query_scalar("SELECT id FROM organizations WHERE name=$1")
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
                    && super::credentials::organization(
                        tx,
                        actor,
                        org,
                        organization_permission,
                        write
                    )
                    .await?,
                Refusal::Denied
            );
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
    let private: Option<bool> =
        sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_optional(&mut *connection)
            .await?;
    if private == Some(false) {
        return Ok(true);
    }
    let (own, org) = snapshot(connection, repo).await?;
    Ok(crate::backend::db::action_settings::fork_workflows_allowed(
        &own, &org,
    ))
}

pub async fn effective(pool: &PgPool, repo: i64) -> Result<(Value, Value)> {
    snapshot(&mut *pool.acquire().await?, repo).await
}

pub async fn settings(
    pool: &PgPool,
    actor: impl Into<Actor>,
    scope: &Scope,
    setting: Setting,
    update: Option<&Value>,
) -> Result<Value> {
    let actor = actor.into();
    super::retry_on_conflict(|| settings_once(pool, &actor, scope, setting, update)).await
}

async fn settings_once(
    pool: &PgPool,
    actor: &Actor,
    scope: &Scope,
    setting: Setting,
    update: Option<&Value>,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    if update.is_none() {
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
    }
    let (table, column, id) = authority(
        &mut tx,
        actor,
        scope,
        update.is_some(),
        setting.organization_permission(),
    )
    .await?;
    let locking = if update.is_some() { " FOR UPDATE" } else { "" };
    let stored: Option<String> = sqlx::query_scalar(&format!(
        "SELECT rest_settings FROM {table} WHERE {column}=$1{locking}"
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
        private = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=$1")
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
        sqlx::query(&format!("INSERT INTO {table}({column},rest_settings) VALUES($1,$2) ON CONFLICT({column}) DO UPDATE SET rest_settings=EXCLUDED.rest_settings,updated_at=NOW()")).bind(id).bind(stored.to_string()).execute(&mut *tx).await?;
    }
    let stored =
        crate::backend::db::action_settings::resolved(stored, &parent, &site, private, maximum);
    tx.commit().await?;
    Ok(stored)
}

pub async fn repositories(
    pool: &PgPool,
    actor: impl Into<Actor>,
    scope: &Scope,
    setting: Setting,
    change: Option<&crate::backend::db::action_settings::SelectionChange>,
    window: (i64, i64),
) -> Result<(i64, Vec<codeyard_db_core::models::Repository>)> {
    let actor = actor.into();
    super::retry_on_conflict(|| repositories_once(pool, &actor, scope, setting, change, window))
        .await
}

async fn repositories_once(
    pool: &PgPool,
    actor: &Actor,
    scope: &Scope,
    setting: Setting,
    change: Option<&crate::backend::db::action_settings::SelectionChange>,
    window: (i64, i64),
) -> Result<(i64, Vec<codeyard_db_core::models::Repository>)> {
    let (offset, limit) = window;
    ensure!(
        matches!(scope, Scope::Organization(_)) && offset >= 0 && (1..=100).contains(&limit),
        Refusal::Invalid
    );
    let mut tx = pool.begin().await?;
    if change.is_none() {
        // One snapshot for the count and the page. Not READ ONLY: the token check takes share
        // locks, which PostgreSQL refuses in a read-only transaction.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
    }
    let (_, _, org) = authority(
        &mut tx,
        actor,
        scope,
        change.is_some(),
        setting.organization_permission(),
    )
    .await?;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT rest_settings FROM org_actions_settings WHERE org_id=$1")
            .bind(org)
            .fetch_optional(&mut *tx)
            .await?;
    let mut stored: Value = serde_json::from_str(stored.as_deref().unwrap_or("{}"))?;
    if let Some(change) = change {
        let mut added =
            crate::backend::db::action_settings::change_selection(&mut stored, setting, change)?;
        added.sort_unstable();
        added.dedup();
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM repositories WHERE owner_org_id=$1 AND id=ANY($2)",
        )
        .bind(org)
        .bind(&added)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(owned == added.len() as i64, Refusal::Invalid);
        sqlx::query("INSERT INTO org_actions_settings(org_id,rest_settings) VALUES($1,$2) ON CONFLICT(org_id) DO UPDATE SET rest_settings=EXCLUDED.rest_settings,updated_at=NOW()")
            .bind(org).bind(stored.to_string()).execute(&mut *tx).await?;
    }
    let ids: Vec<i64> = stored[setting.key()]["selected_repository_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let total = sqlx::query_scalar(
        "SELECT COUNT(*) FROM repositories WHERE owner_org_id=$1 AND id=ANY($2)",
    )
    .bind(org)
    .bind(&ids)
    .fetch_one(&mut *tx)
    .await?;
    let rows = sqlx::query_as("SELECT * FROM repositories WHERE owner_org_id=$1 AND id=ANY($2) ORDER BY id LIMIT $3 OFFSET $4")
        .bind(org).bind(&ids).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((total, rows))
}

pub async fn policies_page(
    pool: &PgPool,
    actor: impl Into<Actor>,
    scope: &Scope,
    has_parents: bool,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<ListedPolicy>)> {
    let actor = actor.into();
    super::retry_on_conflict(|| policies_page_once(pool, &actor, scope, has_parents, offset, limit))
        .await
}

async fn policies_page_once(
    pool: &PgPool,
    actor: &Actor,
    scope: &Scope,
    has_parents: bool,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<ListedPolicy>)> {
    ensure!(offset >= 0 && (1..=100).contains(&limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let (_, column, scope_id) =
        authority(&mut tx, actor, scope, false, "organization_administration").await?;
    let repository = if let Scope::Repository(repo) = scope {
        if has_parents {
            sqlx::query_as::<_, codeyard_db_core::models::Repository>("SELECT r.* FROM repositories r WHERE r.id=$1 AND r.owner_kind='org' AND EXISTS(SELECT 1 FROM actions_policies p WHERE p.org_id=r.owner_org_id)").bind(repo).fetch_optional(&mut *tx).await?
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
        let definitions = super::organizations::properties::definitions_on(&mut tx, org).await?;
        let values =
            super::organizations::properties::values_on(&mut tx, repository.id, &definitions)
                .await?;
        let properties = values.into_iter().filter_map(|value| value.value.map(|stored| (value.property_name, match stored { crate::backend::db::organizations::properties::PropertyValue::String(value) => vec![value], crate::backend::db::organizations::properties::PropertyValue::Strings(values) => values }))).collect();
        let context = crate::backend::db::rulesets::RepositoryContext {
            id: repository.id,
            name: &repository.name,
            default_branch: &repository.default_branch,
            is_private: repository.is_private,
            is_archived: repository.is_archived,
            is_fork: repository.is_fork,
            properties: &properties,
        };
        let mut budget = crate::backend::ci::workflows::WorkflowMatchBudget::new();
        let (mut after, mut total, mut ids) = (0_i64, 0_i64, Vec::new());
        loop {
            let selectors: Vec<PolicySelector> = sqlx::query_as(&format!("SELECT p.id,COALESCE(p.{column}=$1,FALSE) AS own,CASE WHEN p.{column}=$1 THEN NULL ELSE (p.data::jsonb->'conditions')::text END AS conditions FROM actions_policies p WHERE (p.{column}=$1 OR p.org_id=$2) AND p.id>$3 ORDER BY p.id LIMIT 100")).bind(scope_id).bind(org).bind(after).fetch_all(&mut *tx).await?;
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
    let select = "SELECT p.id,p.data,p.created_at,p.updated_at,o.name AS organization FROM actions_policies p LEFT JOIN organizations o ON o.id=p.org_id";
    let (total, rows) = if let Some((total, ids)) = selected {
        let rows = sqlx::query_as(&format!("{select} WHERE p.id=ANY($1) ORDER BY p.id"))
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
        (total, rows)
    } else {
        let total = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM actions_policies WHERE {column}=$1"
        ))
        .bind(scope_id)
        .fetch_one(&mut *tx)
        .await?;
        let rows = sqlx::query_as(&format!(
            "{select} WHERE p.{column}=$1 ORDER BY p.id LIMIT $2 OFFSET $3"
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
    pool: &PgPool,
    actor: impl Into<Actor>,
    scope: &Scope,
    id: Option<i64>,
    change: Option<Option<&Value>>,
) -> Result<Vec<Policy>> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
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
        let lock = if change.is_some() { "FOR UPDATE" } else { "" };
        sqlx::query_as(&format!("SELECT id,data,created_at,updated_at FROM actions_policies WHERE {column}=$1 AND id=$2 {lock}")).bind(scope_id).bind(id).fetch_all(&mut *tx).await?
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
