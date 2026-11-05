use crate::models::OrgRole;
use crate::pool::PgPool;
use crate::queries::organizations::OrganizationOutcome;
use anyhow::Result;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::Transaction;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Denied;
use OrganizationOutcome::Missing;
use OrganizationOutcome::Suspended;
pub async fn begin(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    Ok(pool.begin().await?)
}

// Authorization and projection share a committed snapshot; no locks or effects.
pub async fn begin_read(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

pub async fn read_active(connection: &mut PgConnection, user: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=$1",
    )
    .bind(user)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}

pub async fn read_authority(
    connection: &mut PgConnection,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1")
        .bind(org)
        .fetch_optional(&mut *connection)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    if !read_active(connection, actor).await? {
        return Ok(Denied);
    }
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM org_memberships WHERE org_id=$1 AND user_id=$2")
            .bind(org)
            .bind(actor)
            .fetch_optional(connection)
            .await?;
    Ok(match role.and_then(|role| role.parse().ok()) {
        Some(role) => Applied(role),
        None => Denied,
    })
}

#[macro_export]
macro_rules! require_read_org_postgres {
    ($tx:ident, $actor:ident, $org:ident) => {
        match $crate::queries::postgres::organizations::authority::read_authority(
            &mut $tx, $actor, $org,
        )
        .await?
        {
            $crate::queries::organizations::OrganizationOutcome::Applied(role) => role,
            $crate::queries::organizations::OrganizationOutcome::Missing => {
                return Ok($crate::queries::organizations::OrganizationOutcome::Missing)
            }
            _ => return Ok($crate::queries::organizations::OrganizationOutcome::Denied),
        }
    };
}

pub async fn active(connection: &mut PgConnection, user: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=$1 FOR SHARE",
    )
    .bind(user)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}

pub async fn membership_role(
    connection: &mut PgConnection,
    org: i64,
    user: i64,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT role FROM org_memberships WHERE org_id=$1 AND user_id=$2 FOR UPDATE",
    )
    .bind(org)
    .bind(user)
    .fetch_optional(connection)
    .await?)
}

pub async fn authority(
    connection: &mut PgConnection,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    crate::queries::postgres::users::transitions::lock_accounts_shared(connection, &[actor])
        .await?;
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org)
        .fetch_optional(&mut *connection)
        .await?
        .is_none()
    {
        return Ok(Missing);
    }
    if !active(connection, actor).await? {
        return Ok(Denied);
    }
    let Some(role) = membership_role(connection, org, actor)
        .await?
        .and_then(|role| role.parse().ok())
    else {
        return Ok(Denied);
    };
    // Banning/restricting the last owner remains possible. Tenant writes are
    // suspended instead of treating restricted owners as usable governance.
    if !remaining_active_owner(connection, org, -1).await? {
        return Ok(Suspended);
    }
    Ok(Applied(role))
}

pub async fn remaining_active_owner(
    connection: &mut PgConnection,
    org: i64,
    target: i64,
) -> Result<bool> {
    // The organization row pins ownership and owner restrictions. Taking owner
    // account rows here would invert account restrictions' account-before-org order.
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.role='owner' AND m.user_id<>$2 AND NOT u.is_banned AND NOT u.must_change_password)")
        .bind(org).bind(target).fetch_one(connection).await?)
}
