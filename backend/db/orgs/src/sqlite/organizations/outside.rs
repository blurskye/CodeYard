use crate::sqlite::organizations::authority::cancel_actor;
use crate::sqlite::organizations::authority::credential;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::User;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::sqlite::organizations::authority::begin_read;
use codeyard_db_core::queries::sqlite::organizations::authority::membership_role;
use codeyard_db_core::require_read_org_sqlite;
use sqlx::SqlitePool;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Conflict;
use OrganizationOutcome::Denied;
use OrganizationOutcome::Missing;

pub async fn list(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    filter: &str,
    after: i64,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<User>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org_sqlite!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !matches!(filter, "all" | "2fa_disabled" | "2fa_insecure") {
        return Ok(Conflict);
    }
    let rows = sqlx::query_as("SELECT u.* FROM users u WHERE u.id>?1 AND EXISTS(SELECT 1 FROM repo_access a JOIN repositories r ON r.id=a.repo_id WHERE a.user_id=u.id AND a.access_level BETWEEN 1 AND 5 AND r.owner_kind='org' AND r.owner_org_id=?2 AND r.owner_user_id IS NULL) AND NOT EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=?2 AND m.user_id=u.id) AND (?3='all' OR (?3='2fa_disabled' AND NOT u.two_factor_enabled)) ORDER BY u.id LIMIT ?4 OFFSET ?5")
        .bind(after).bind(org).bind(filter).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn remove(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    target: i64,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !credential(&mut tx, &grant, org, "members").await? {
        return Ok(Denied);
    }
    if membership_role(&mut tx, org, target).await?.is_some() {
        return Ok(Conflict);
    }
    let deleted = sqlx::query("DELETE FROM repo_access WHERE user_id=?1 AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?2)")
        .bind(target).bind(org).execute(&mut *tx).await?.rows_affected();
    if deleted == 0 {
        return Ok(Missing);
    }
    cancel_actor(&mut tx, org, target).await?;
    tx.commit().await?;
    Ok(Applied(()))
}
