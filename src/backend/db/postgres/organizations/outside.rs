use super::authority::*;
use crate::backend::db::{
    models::{OrgRole, User},
    organizations::OrganizationOutcome,
};
use anyhow::Result;
use sqlx::PgPool;
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

pub async fn list(
    pool: &PgPool,
    actor: i64,
    org: i64,
    filter: &str,
    after: i64,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<User>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !matches!(filter, "all" | "2fa_disabled" | "2fa_insecure") {
        return Ok(Conflict);
    }
    let rows = sqlx::query_as("SELECT u.* FROM users u WHERE u.id>$1 AND EXISTS(SELECT 1 FROM repo_access a JOIN repositories r ON r.id=a.repo_id WHERE a.user_id=u.id AND a.access_level BETWEEN 1 AND 5 AND r.owner_kind='org' AND r.owner_org_id=$2 AND r.owner_user_id IS NULL) AND NOT EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=$2 AND m.user_id=u.id) AND ($3='all' OR ($3='2fa_disabled' AND NOT u.two_factor_enabled)) ORDER BY u.id LIMIT $4 OFFSET $5")
        .bind(after).bind(org).bind(filter).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn remove(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !credential(&mut tx, &grant, org, "members").await? { return Ok(Denied); }
    active(&mut tx, target).await?;
    if membership_role(&mut tx, org, target).await?.is_some() {
        return Ok(Conflict);
    }
    let deleted = sqlx::query("DELETE FROM repo_access WHERE user_id=$1 AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$2)")
        .bind(target).bind(org).execute(&mut *tx).await?.rows_affected();
    if deleted == 0 {
        return Ok(Missing);
    }
    cancel_actor(&mut tx, org, target).await?;
    tx.commit().await?;
    Ok(Applied(()))
}
