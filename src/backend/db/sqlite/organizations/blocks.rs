use super::authority::*;
use crate::backend::db::organizations::OrganizationOutcome;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::User;
use sqlx::{SqliteConnection, SqlitePool};
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

pub(super) async fn target_blocked(
    connection: &mut SqliteConnection,
    org: i64,
    target: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organization_blocks WHERE org_id=?1 AND user_id=?2)",
    )
    .bind(org)
    .bind(target)
    .fetch_one(connection)
    .await?)
}

pub async fn list(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    after: i64,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<User>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let rows=sqlx::query_as("SELECT u.* FROM users u JOIN organization_blocks b ON b.user_id=u.id WHERE b.org_id=?1 AND u.id>?2 ORDER BY u.id LIMIT ?3 OFFSET ?4").bind(org).bind(after).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn check(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<bool>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let blocked = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organization_blocks WHERE org_id=?1 AND user_id=?2)",
    )
    .bind(org)
    .bind(target)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(blocked))
}
pub async fn change(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
    blocked: bool,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !super::super::credentials::live(&mut tx, &grant, true).await? {
        return Ok(Denied);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1)")
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Ok(Missing);
    }
    if !blocked {
        sqlx::query("DELETE FROM organization_blocks WHERE org_id=?1 AND user_id=?2")
            .bind(org)
            .bind(target)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(Applied(()));
    }
    if membership_role(&mut tx, org, target).await?.is_some() {
        return Ok(Conflict);
    }
    sqlx::query(
        "INSERT INTO organization_blocks(org_id,user_id) VALUES(?1,?2) ON CONFLICT DO NOTHING",
    )
    .bind(org)
    .bind(target)
    .execute(&mut *tx)
    .await?;
    let invitations: Vec<i64>=sqlx::query_scalar("SELECT i.id FROM organization_invitations i WHERE i.org_id=?1 AND ((i.email IS NULL AND i.user_id=?2) OR EXISTS(SELECT 1 FROM users u WHERE u.id=?2 AND u.email_verified AND LOWER(u.email)=i.email) OR EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=?2 AND e.verified AND LOWER(e.email)=i.email)) ORDER BY i.id").bind(org).bind(target).fetch_all(&mut *tx).await?;
    for invitation in invitations {
        sqlx::query("DELETE FROM email_outbox WHERE organization_invitation_id=?1")
            .bind(invitation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM organization_invitations WHERE id=?1")
            .bind(invitation)
            .execute(&mut *tx)
            .await?;
    }
    let repositories = "SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?1";
    for table in ["stars", "watches", "repo_access"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE user_id=?2 AND repo_id IN ({repositories})"
        ))
        .bind(org)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(&format!("DELETE FROM issue_assignees WHERE user_id=?2 AND issue_id IN (SELECT id FROM issues WHERE repo_id IN ({repositories}))"))
        .bind(org).bind(target).execute(&mut *tx).await?;
    for table in ["pr_assignees", "pr_reviewers"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id=?2 AND pull_request_id IN (SELECT id FROM pull_requests WHERE repo_id IN ({repositories}))"))
            .bind(org).bind(target).execute(&mut *tx).await?;
    }
    sqlx::query(&format!("DELETE FROM notifications WHERE (user_id=?2 OR actor_id=?2) AND repo_id IN ({repositories})"))
        .bind(org).bind(target).execute(&mut *tx).await?;
    sqlx::query(&format!("UPDATE repository_invitations SET state='cancelled' WHERE invitee_id=?2 AND state='pending' AND repo_id IN ({repositories})"))
        .bind(org).bind(target).execute(&mut *tx).await?;
    cancel_actor(&mut tx, org, target).await?;
    tx.commit().await?;
    Ok(Applied(()))
}
