use super::authority::*;
use crate::backend::db::{
    models::OrgRole,
    organizations::{
        external_properties::{self, Installation, RepositoryValue},
        OrganizationOutcome,
    },
};
use anyhow::Result;
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
    let row = sqlx::query_as("INSERT INTO organization_external_property_installations(org_id,display_name,installation_id) VALUES($1,$2,$3) ON CONFLICT(org_id,display_name) DO UPDATE SET installation_id=COALESCE(excluded.installation_id,organization_external_property_installations.installation_id) RETURNING display_name,COALESCE(installation_id,id) AS installation_id").bind(org).bind(name).bind(installation).fetch_one(&mut *tx).await?;
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
    for value in values {
        if !external_properties::valid_name(&value.property_name)
            || !external_properties::valid_value(&value.value)
        {
            return Ok(Conflict);
        }
        let repo: Option<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 AND owner_user_id IS NULL AND name=$2 FOR SHARE").bind(org).bind(&value.repository_name).fetch_optional(&mut *tx).await?;
        let Some(repo) = repo else {
            return Ok(Conflict);
        };
        sqlx::query("INSERT INTO organization_external_property_values(org_id,repo_id,property_name,value_json) VALUES($1,$2,$3,$4) ON CONFLICT(org_id,repo_id,property_name) DO UPDATE SET value_json=excluded.value_json").bind(org).bind(repo).bind(&value.property_name).bind(serde_json::to_string(&value.value)?).execute(&mut *tx).await?;
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
