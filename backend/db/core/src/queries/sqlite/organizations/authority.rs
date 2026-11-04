use crate::models::OrgRole;
use crate::queries::organizations::OrganizationOutcome;
use anyhow::Result;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::Transaction;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Denied;
use OrganizationOutcome::Missing;
use OrganizationOutcome::Suspended;
pub async fn begin(pool: &SqlitePool) -> Result<Transaction<'_, Sqlite>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

pub async fn begin_read(pool: &SqlitePool) -> Result<Transaction<'_, Sqlite>> {
    // Deferred BEGIN pins a snapshot at the first SELECT; read helpers never write.
    Ok(pool.begin().await?)
}

pub async fn active(connection: &mut SqliteConnection, user: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=?1",
    )
    .bind(user)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}

pub async fn membership_role(
    connection: &mut SqliteConnection,
    org: i64,
    user: i64,
) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT role FROM org_memberships WHERE org_id=?1 AND user_id=?2")
            .bind(org)
            .bind(user)
            .fetch_optional(connection)
            .await?,
    )
}

pub async fn read_authority(
    connection: &mut SqliteConnection,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    if sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=?1")
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
    Ok(
        match membership_role(connection, org, actor)
            .await?
            .and_then(|role| role.parse().ok())
        {
            Some(role) => Applied(role),
            None => Denied,
        },
    )
}

pub async fn authority(
    connection: &mut SqliteConnection,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<OrgRole>> {
    let role = match read_authority(connection, actor, org).await? {
        Applied(role) => role,
        other => return Ok(other),
    };
    if !remaining_active_owner(connection, org, -1).await? {
        return Ok(Suspended);
    }
    Ok(Applied(role))
}

#[macro_export]
macro_rules! require_read_org_sqlite {
    ($tx:ident, $actor:ident, $org:ident) => {
        match $crate::queries::sqlite::organizations::authority::read_authority(
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

pub async fn remaining_active_owner(
    connection: &mut SqliteConnection,
    org: i64,
    target: i64,
) -> Result<bool> {
    // Writer ownership retains all user/membership facts through the mutation.
    // Restriction/deletion governance still needs parent policy and wiring.
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=?1 AND m.role='owner' AND m.user_id<>?2 AND NOT u.is_banned AND NOT u.must_change_password)")
        .bind(org).bind(target).fetch_one(connection).await?)
}
