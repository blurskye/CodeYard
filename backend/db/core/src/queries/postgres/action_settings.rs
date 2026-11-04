use crate::queries::action_settings::Refusal;
use crate::queries::action_settings::Scope;
use crate::queries::action_settings::Setting;
use crate::queries::credentials::Actor;
use crate::queries::organizations::OrganizationOutcome;
use anyhow::ensure;
use anyhow::Result;
use serde_json::Value;
use sqlx::PgConnection;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;
pub async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    actor: &Actor,
    scope: &Scope,
    write: bool,
    organization_permission: &str,
) -> Result<(&'static str, &'static str, i64)> {
    match scope {
        Scope::Repository(repo) => {
            let allowed = if write {
                crate::queries::postgres::discussions::lock_repository_mutation_authority(
                    tx,
                    actor.user_id,
                    *repo,
                )
                .await?
            } else {
                crate::queries::postgres::discussions::lock_policy_authority(
                    tx,
                    actor.user_id,
                    *repo,
                    false,
                )
                .await?
            };
            ensure!(
                allowed
                    && crate::queries::postgres::credentials::repository(
                        tx,
                        actor,
                        *repo,
                        "administration",
                        write
                    )
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
                crate::queries::postgres::organizations::authority::authority(
                    tx,
                    actor.user_id,
                    org,
                )
                .await?
            } else {
                crate::queries::postgres::organizations::authority::read_authority(
                    tx,
                    actor.user_id,
                    org,
                )
                .await?
            };
            ensure!(
                matches!(role, OrganizationOutcome::Applied(role) if crate::queries::organizations::administers(role))
                    && crate::queries::postgres::credentials::organization(
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

pub async fn snapshot(connection: &mut PgConnection, repo: i64) -> Result<(Value, Value)> {
    let (own, org): (String, String) = sqlx::query_as("SELECT COALESCE(s.rest_settings,'{}'),COALESCE(o.rest_settings,'{}') FROM repositories r LEFT JOIN repo_actions_settings s ON s.repo_id=r.id LEFT JOIN org_actions_settings o ON o.org_id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=$1").bind(repo).fetch_optional(connection).await?.ok_or(Refusal::Missing)?;
    Ok((serde_json::from_str(&own)?, serde_json::from_str(&org)?))
}

/// Whether a fork pull request run may be admitted or claimed for this repository.
pub async fn fork_runs_allowed(connection: &mut PgConnection, repo: i64) -> Result<bool> {
    let private: Option<bool> =
        sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_optional(&mut *connection)
            .await?;
    if private == Some(false) {
        return Ok(true);
    }
    let (own, org) = snapshot(connection, repo).await?;
    Ok(crate::queries::action_settings::fork_workflows_allowed(
        &own, &org,
    ))
}

pub async fn repositories(
    pool: &PgPool,
    actor: impl Into<Actor>,
    scope: &Scope,
    setting: Setting,
    change: Option<&crate::queries::action_settings::SelectionChange>,
    window: (i64, i64),
) -> Result<(i64, Vec<crate::models::Repository>)> {
    let actor = actor.into();
    crate::queries::postgres::retry_on_conflict(|| {
        repositories_once(pool, &actor, scope, setting, change, window)
    })
    .await
}

async fn repositories_once(
    pool: &PgPool,
    actor: &Actor,
    scope: &Scope,
    setting: Setting,
    change: Option<&crate::queries::action_settings::SelectionChange>,
    window: (i64, i64),
) -> Result<(i64, Vec<crate::models::Repository>)> {
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
            crate::queries::action_settings::change_selection(&mut stored, setting, change)?;
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
