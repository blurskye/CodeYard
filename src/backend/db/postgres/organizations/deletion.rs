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
        crate::backend::db::postgres::files::delete_unreferenced_file_in_transaction(
            &mut tx, avatar,
        )
        .await?;
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

/// Capture the initial repository set so concurrent creations survive the deletion.
pub async fn list_organization_deletion_repositories(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Vec<i64>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let repository = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 ORDER BY id",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(repository))
}

/// An owner may delete even archived repositories, but only while they belong to this organization.
pub async fn delete_organization_repository(
    pool: &PgPool,
    actor: i64,
    org: i64,
    repo: i64,
) -> Result<OrganizationOutcome<crate::backend::db::models::Repository>> {
    let mut tx = begin(pool).await?;
    // Deletion locks the organization before fork ancestors; exclude notification reads.
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    crate::backend::db::postgres::repositories::records::lock_fork_ancestors(
        &mut tx,
        repo,
        "FOR NO KEY UPDATE",
    )
    .await?;
    let files: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT DISTINCT a.file_id FROM release_assets a JOIN releases r ON r.id=a.release_id WHERE r.repo_id=$1 ORDER BY a.file_id")
        .bind(repo).fetch_all(&mut *tx).await?;
    let deleted = sqlx::query_as("DELETE FROM repositories WHERE id=$1 AND owner_kind='org' AND owner_org_id=$2 AND owner_user_id IS NULL RETURNING *")
        .bind(repo).bind(org).fetch_optional(&mut *tx).await?;
    let Some(deleted) = deleted else {
        return Ok(Conflict);
    };
    for file in files {
        crate::backend::db::postgres::files::delete_unreferenced_file_in_transaction(&mut tx, file)
            .await?;
    }
    tx.commit().await?;
    Ok(Applied(deleted))
}

async fn repository_count(connection: &mut PgConnection, org: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM repositories WHERE owner_kind='org' AND owner_org_id=$1",
    )
    .bind(org)
    .fetch_one(connection)
    .await?)
}
