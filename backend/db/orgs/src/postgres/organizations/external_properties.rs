use crate::organizations::external_properties::Installation;
use crate::organizations::external_properties::RepositoryValue;
use crate::postgres::organizations::authority::credential;
use crate::postgres::organizations::authority::require_org;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::postgres::organizations::authority::begin;
use codeyard_db_core::queries::postgres::organizations::authority::begin_read;
use codeyard_db_core::require_read_org_postgres;
use sqlx::PgPool;
use std::collections::HashMap;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Conflict;
use OrganizationOutcome::Denied;

pub async fn installations(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Vec<Installation>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org_postgres!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let rows = sqlx::query_as("SELECT display_name,installation_id FROM organization_external_property_installations WHERE org_id=$1 ORDER BY id").bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}

pub async fn register(
    pool: &PgPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    name: &str,
    installation: Option<i64>,
) -> Result<OrganizationOutcome<Installation>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner
        || !credential(&mut tx, &grant, org, "organization_custom_properties").await?
    {
        return Ok(Denied);
    }
    let Some(installation) = installation else {
        return Ok(Conflict);
    };
    let permissions: Option<String> = sqlx::query_scalar("SELECT a.permissions FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE i.id=$1 AND i.org_id=$2 AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password FOR SHARE OF i,a,owner,bot")
        .bind(installation).bind(org).fetch_optional(&mut *tx).await?;
    let Some(permissions) = permissions else {
        return Ok(Conflict);
    };
    let permissions: serde_json::Value = serde_json::from_str(&permissions)?;
    if !permissions
        .get("organization_external_properties_for_repos")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|level| matches!(level, "write" | "admin"))
    {
        return Ok(Conflict);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_external_property_installations WHERE org_id=$1 AND (display_name=$2 OR installation_id=$3))").bind(org).bind(name).bind(installation).fetch_one(&mut *tx).await?;
    if exists {
        return Ok(Conflict);
    }
    if name.is_empty()
        || name.len() > 15
        || !name.bytes().all(|ch| ch.is_ascii_alphanumeric())
        || installation <= 0
    {
        return Ok(Conflict);
    }
    let row: Option<Installation> = sqlx::query_as("INSERT INTO organization_external_property_installations(org_id,display_name,installation_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING RETURNING display_name,installation_id").bind(org).bind(name).bind(installation).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(Conflict);
    };
    tx.commit().await?;
    Ok(Applied(row))
}

pub async fn definitions(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Vec<String>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org_postgres!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let rows = sqlx::query_scalar("SELECT DISTINCT v.property_name FROM organization_external_property_values v JOIN repositories r ON r.id=v.repo_id WHERE v.org_id=$1 AND r.owner_kind='org' AND r.owner_org_id=$1 AND r.owner_user_id IS NULL ORDER BY v.property_name").bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}

pub async fn set_values(
    pool: &PgPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    values: &[RepositoryValue],
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner
        || !credential(&mut tx, &grant, org, "organization_custom_properties").await?
    {
        return Ok(Denied);
    }
    if values.len() > 3000 {
        return Ok(Conflict);
    }
    let Some(changes) = crate::organizations::external_properties::changes(values)? else {
        return Ok(Conflict);
    };
    let names: Vec<String> = changes
        .repositories
        .iter()
        .map(|name| name.to_string())
        .collect();
    let found: Vec<(i64, String)> = sqlx::query_as("SELECT id,name FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 AND owner_user_id IS NULL AND name=ANY($2) ORDER BY id FOR SHARE").bind(org).bind(&names).fetch_all(&mut *tx).await?;
    let ids: HashMap<&str, i64> = found
        .iter()
        .map(|(id, name)| (name.as_str(), *id))
        .collect();
    if ids.len() != names.len() {
        return Ok(Conflict);
    }
    if !changes.sets.is_empty() {
        let repos: Vec<i64> = changes.sets.iter().map(|(repo, _, _)| ids[*repo]).collect();
        let properties: Vec<String> = changes
            .sets
            .iter()
            .map(|(_, property, _)| property.to_string())
            .collect();
        let json: Vec<String> = changes
            .sets
            .iter()
            .map(|(_, _, json)| json.clone())
            .collect();
        sqlx::query("INSERT INTO organization_external_property_values(org_id,repo_id,property_name,value_json) SELECT $1,t.repo_id,t.property_name,t.value_json FROM UNNEST($2::bigint[],$3::text[],$4::text[]) AS t(repo_id,property_name,value_json) ON CONFLICT(org_id,repo_id,property_name) DO UPDATE SET value_json=excluded.value_json").bind(org).bind(&repos).bind(&properties).bind(&json).execute(&mut *tx).await?;
    }
    if !changes.unsets.is_empty() {
        let repos: Vec<i64> = changes.unsets.iter().map(|(repo, _)| ids[*repo]).collect();
        let properties: Vec<String> = changes
            .unsets
            .iter()
            .map(|(_, property)| property.to_string())
            .collect();
        sqlx::query("DELETE FROM organization_external_property_values v WHERE v.org_id=$1 AND EXISTS(SELECT 1 FROM UNNEST($2::bigint[],$3::text[]) AS t(repo_id,property_name) WHERE t.repo_id=v.repo_id AND t.property_name=v.property_name)").bind(org).bind(&repos).bind(&properties).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Applied(()))
}

pub async fn delete_values(
    pool: &PgPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    property: &str,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner
        || !credential(&mut tx, &grant, org, "organization_custom_properties").await?
    {
        return Ok(Denied);
    }
    if !crate::organizations::external_properties::valid_name(property) {
        return Ok(Conflict);
    }
    sqlx::query(
        "DELETE FROM organization_external_property_values WHERE org_id=$1 AND property_name=$2",
    )
    .bind(org)
    .bind(property)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(()))
}
