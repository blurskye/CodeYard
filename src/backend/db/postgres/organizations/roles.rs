use super::authority::*;
use crate::backend::db::{
    models::{OrgRole, Team},
    organizations::{
        roles::{OrganizationRole, RoleTarget, UserAssignment},
        OrganizationOutcome,
    },
};
use anyhow::Result;
use sqlx::{PgConnection, PgPool};
use OrganizationOutcome::{Applied, Conflict, Denied, Missing};

async fn role_exists(c: &mut PgConnection, id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_roles WHERE id=$1 AND base_level BETWEEN 1 AND 5)").bind(id).fetch_one(c).await?)
}
pub async fn can_manage_security(pool: &PgPool, actor: i64, org: i64) -> Result<bool> {
    manages_security(&mut *pool.acquire().await?, actor, org).await
}
/// Whether `actor` holds the organization's security-manager role, directly or through a team.
pub(crate) async fn manages_security(c: &mut PgConnection, actor: i64, org: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations o JOIN org_memberships m ON m.org_id=o.id JOIN users u ON u.id=m.user_id WHERE o.id=$2 AND u.id=$1 AND m.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM organization_blocks b WHERE b.org_id=o.id AND b.user_id=u.id) AND EXISTS(SELECT 1 FROM org_memberships owner JOIN users active ON active.id=owner.user_id WHERE owner.org_id=o.id AND owner.role='owner' AND NOT active.is_banned AND NOT active.must_change_password) AND (m.role='owner' OR EXISTS(SELECT 1 FROM organization_role_assignments a JOIN organization_roles role ON role.id=a.role_id AND role.base_level=1 WHERE a.org_id=o.id AND a.role_id=$3 AND ((a.user_id=u.id AND a.team_id IS NULL) OR (a.user_id IS NULL AND EXISTS(SELECT 1 FROM teams t JOIN effective_team_memberships tm ON tm.team_id=t.id WHERE t.id=a.team_id AND t.org_id=a.org_id AND t.privacy IN ('visible','secret') AND tm.user_id=u.id AND tm.role IN ('member','maintainer')))))))").bind(actor).bind(org).bind(crate::backend::db::organizations::roles::SECURITY_MANAGER_ROLE_ID).fetch_one(c).await?)
}
pub async fn catalog(
    pool: &PgPool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Vec<OrganizationRole>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let roles = sqlx::query_as(
        "SELECT * FROM organization_roles WHERE base_level BETWEEN 1 AND 5 ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied(roles))
}
pub async fn users(
    pool: &PgPool,
    actor: i64,
    org: i64,
    role: i64,
    after: i64,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<UserAssignment>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !role_exists(&mut tx, role).await? {
        return Ok(Missing);
    }
    let mut users:Vec<UserAssignment>=sqlx::query_as("SELECT u.*,EXISTS(SELECT 1 FROM organization_role_assignments a WHERE a.org_id=$1 AND a.role_id=$2 AND a.user_id=u.id) AS direct FROM users u JOIN org_memberships m ON m.user_id=u.id AND m.org_id=$1 AND m.role IN ('owner','admin','member') WHERE u.id>$3 AND (EXISTS(SELECT 1 FROM organization_role_assignments a WHERE a.org_id=$1 AND a.role_id=$2 AND a.user_id=u.id) OR EXISTS(SELECT 1 FROM organization_role_assignments a JOIN teams t ON t.id=a.team_id AND t.org_id=a.org_id JOIN effective_team_memberships tm ON tm.team_id=t.id AND tm.user_id=u.id AND tm.role IN ('member','maintainer') WHERE a.org_id=$1 AND a.role_id=$2 AND t.privacy IN ('visible','secret'))) ORDER BY u.id LIMIT $4 OFFSET $5").bind(org).bind(role).bind(after).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    for user in &mut users {
        user.inherited_from=sqlx::query_as("SELECT t.* FROM teams t JOIN organization_role_assignments a ON a.team_id=t.id AND a.org_id=t.org_id AND a.role_id=$2 JOIN effective_team_memberships m ON m.team_id=t.id AND m.user_id=$3 AND m.role IN ('member','maintainer') WHERE t.org_id=$1 AND t.privacy IN ('visible','secret') ORDER BY t.id").bind(org).bind(role).bind(user.user.id).fetch_all(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Applied(users))
}
pub async fn teams(
    pool: &PgPool,
    actor: i64,
    org: i64,
    role: i64,
    after: i64,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<Team>>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !role_exists(&mut tx, role).await? {
        return Ok(Missing);
    }
    let teams=sqlx::query_as("SELECT t.* FROM teams t JOIN organization_role_assignments a ON a.team_id=t.id AND a.org_id=t.org_id WHERE t.org_id=$1 AND a.role_id=$2 AND t.privacy IN ('visible','secret') AND t.id>$3 ORDER BY t.id LIMIT $4 OFFSET $5").bind(org).bind(role).bind(after).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(teams))
}
pub async fn change(
    pool: &PgPool,
    actor: i64,
    org: i64,
    target: &RoleTarget,
    role: Option<i64>,
    assigned: bool,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    crate::backend::db::postgres::users::transitions::lock_membership(&mut tx).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let (user, team) = match target {
        RoleTarget::User(name) => {
            let id: Option<i64> =
                sqlx::query_scalar("SELECT id FROM users WHERE username=$1 FOR SHARE")
                    .bind(name)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some(id) = id {
                if assigned {
                    if !active(&mut tx, id).await? {
                        return Ok(Denied);
                    }
                    match membership_role(&mut tx, org, id).await?.as_deref() {
                        None => return Ok(Conflict),
                        Some("owner" | "admin" | "member") => {}
                        Some(_) => return Ok(Denied),
                    }
                }
            }
            (id, None)
        }
        RoleTarget::Team(slug) => {
            let team: Option<(i64, String)> = sqlx::query_as(
                "SELECT id,privacy FROM teams WHERE org_id=$1 AND slug=$2 FOR SHARE",
            )
            .bind(org)
            .bind(slug)
            .fetch_optional(&mut *tx)
            .await?;
            if assigned
                && team
                    .as_ref()
                    .is_some_and(|(_, privacy)| !matches!(privacy.as_str(), "visible" | "secret"))
            {
                return Ok(Denied);
            }
            (None, team.map(|(id, _)| id))
        }
    };
    if user.is_none() && team.is_none() {
        if assigned {
            return Ok(Missing);
        }
        tx.commit().await?;
        return Ok(Applied(()));
    }
    if assigned {
        let Some(role) = role else {
            return Ok(Conflict);
        };
        if !role_exists(&mut tx, role).await? {
            return Ok(Missing);
        }
        sqlx::query("INSERT INTO organization_role_assignments(org_id,role_id,user_id,team_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(org).bind(role).bind(user).bind(team).execute(&mut *tx).await?;
    } else {
        let write_effect:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_role_assignments a JOIN organization_roles r ON r.id=a.role_id AND r.base_level BETWEEN 3 AND 5 WHERE a.org_id=$1 AND ($2::BIGINT IS NULL OR a.role_id=$2) AND (a.user_id=$3 OR a.team_id=$4))").bind(org).bind(role).bind(user).bind(team).fetch_one(&mut *tx).await?;
        let deleted=sqlx::query("DELETE FROM organization_role_assignments WHERE org_id=$1 AND ($2::BIGINT IS NULL OR role_id=$2) AND (user_id=$3 OR team_id=$4)").bind(org).bind(role).bind(user).bind(team).execute(&mut *tx).await?.rows_affected();
        if deleted > 0 && write_effect {
            let users = if let Some(user) = user {
                vec![user]
            } else {
                sqlx::query_scalar(
                    "SELECT user_id FROM effective_team_memberships WHERE team_id=$1 ORDER BY user_id",
                )
                .bind(team)
                .fetch_all(&mut *tx)
                .await?
            };
            for user in users {
                cancel_actor(&mut tx, org, user).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(Applied(()))
}
