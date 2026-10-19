use crate::backend::db::{models::OrgRole, organizations::*};
use anyhow::Result;
use sqlx::{PgConnection, PgPool};
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

use super::authority::*;

pub async fn delete_organization(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    let role = require_org!(tx, actor, org);
    if role != OrgRole::Owner {
        return Ok(Denied);
    }
    // The row lock keeps a concurrent repository insert from referencing the organization mid-delete.
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    if repository_count(&mut tx, org).await? > 0 {
        return Ok(Conflict);
    }
    let avatar: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT avatar_file_id FROM organizations WHERE id=$1")
            .bind(org)
            .fetch_one(&mut *tx)
            .await?;
    // Teams, memberships, secrets and audit rows go with the organization by cascade.
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    if let Some(avatar) = avatar {
        crate::backend::db::postgres::files::delete_replaced_picture(&mut tx, avatar).await?;
    }
    tx.commit().await?;
    Ok(Applied(()))
}

pub async fn count_organization_repositories(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<i64>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let count = repository_count(&mut tx, org).await?;
    tx.commit().await?;
    Ok(Applied(count))
}

/// One repository the organization still owns, for the deletion loop. `None` when none remain.
pub async fn next_organization_repository(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Option<i64>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let repository = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 ORDER BY id LIMIT 1",
    )
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(repository))
}

async fn repository_count(connection: &mut PgConnection, org: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM repositories WHERE owner_kind='org' AND owner_org_id=$1",
    )
    .bind(org)
    .fetch_one(connection)
    .await?)
}
