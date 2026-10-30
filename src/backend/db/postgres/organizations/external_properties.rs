use super::authority::*;
use crate::backend::db::{
    models::OrgRole,
    organizations::{
        external_properties::{self, Installation, RepositoryValue},
        OrganizationOutcome,
    },
};
use anyhow::Result;
use std::collections::HashMap;
use sqlx::PgPool;
use OrganizationOutcome::{Applied, Conflict, Denied};

pub async fn installations(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Vec<Installation>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let rows = sqlx::query_as("SELECT display_name,COALESCE(installation_id,id) AS installation_id FROM organization_external_property_installations WHERE org_id=$1 ORDER BY id").bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}

pub async fn register(
    pool: &PgPool,
    actor: i64,
    org: i64,
    name: &str,
    installation: Option<i64>,
) -> Result<OrganizationOutcome<Installation>> {
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if name.is_empty()
        || name.len() > 15
        || !name.bytes().all(|ch| ch.is_ascii_alphanumeric())
        || installation.is_some_and(|id| id <= 0)
    {
        return Ok(Conflict);
    }
    let row: Option<Installation> = sqlx::query_as("INSERT INTO organization_external_property_installations(org_id,display_name,installation_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING RETURNING display_name,COALESCE(installation_id,id) AS installation_id").bind(org).bind(name).bind(installation).fetch_optional(&mut *tx).await?;
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
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let rows = sqlx::query_scalar("SELECT DISTINCT v.property_name FROM organization_external_property_values v JOIN repositories r ON r.id=v.repo_id WHERE v.org_id=$1 AND r.owner_kind='org' AND r.owner_org_id=$1 AND r.owner_user_id IS NULL ORDER BY v.property_name").bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}

pub async fn set_values(
    pool: &PgPool,
    actor: i64,
    org: i64,
    values: &[RepositoryValue],
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if values.len() > 3000 {
        return Ok(Conflict);
    }
    let Some(changes) = external_properties::changes(values)? else {
        return Ok(Conflict);
    };
    let names: Vec<String> = changes.repositories.iter().map(|name| name.to_string()).collect();
    let found: Vec<(i64, String)> = sqlx::query_as("SELECT id,name FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 AND owner_user_id IS NULL AND name=ANY($2) ORDER BY id FOR SHARE").bind(org).bind(&names).fetch_all(&mut *tx).await?;
    let ids: HashMap<&str, i64> = found.iter().map(|(id, name)| (name.as_str(), *id)).collect();
    if ids.len() != names.len() {
        return Ok(Conflict);
    }
    if !changes.sets.is_empty() {
        let repos: Vec<i64> = changes.sets.iter().map(|(repo, _, _)| ids[*repo]).collect();
        let properties: Vec<String> = changes.sets.iter().map(|(_, property, _)| property.to_string()).collect();
        let json: Vec<String> = changes.sets.iter().map(|(_, _, json)| json.clone()).collect();
        sqlx::query("INSERT INTO organization_external_property_values(org_id,repo_id,property_name,value_json) SELECT $1,t.repo_id,t.property_name,t.value_json FROM UNNEST($2::bigint[],$3::text[],$4::text[]) AS t(repo_id,property_name,value_json) ON CONFLICT(org_id,repo_id,property_name) DO UPDATE SET value_json=excluded.value_json").bind(org).bind(&repos).bind(&properties).bind(&json).execute(&mut *tx).await?;
    }
    if !changes.unsets.is_empty() {
        let repos: Vec<i64> = changes.unsets.iter().map(|(repo, _)| ids[*repo]).collect();
        let properties: Vec<String> = changes.unsets.iter().map(|(_, property)| property.to_string()).collect();
        sqlx::query("DELETE FROM organization_external_property_values v WHERE v.org_id=$1 AND EXISTS(SELECT 1 FROM UNNEST($2::bigint[],$3::text[]) AS t(repo_id,property_name) WHERE t.repo_id=v.repo_id AND t.property_name=v.property_name)").bind(org).bind(&repos).bind(&properties).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Applied(()))
}

pub async fn delete_values(
    pool: &PgPool,
    actor: i64,
    org: i64,
    property: &str,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !external_properties::valid_name(property) {
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
