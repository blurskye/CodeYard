use crate::organizations::discovery::MembershipState;
use crate::postgres::organizations::authority::credential;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Organization;
use codeyard_db_core::models::User;
use codeyard_db_core::pool::PgPool;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::postgres::organizations::authority::begin;
use codeyard_db_core::queries::postgres::organizations::authority::begin_read;
use codeyard_db_core::queries::postgres::organizations::authority::read_authority;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Denied;

pub async fn list_organizations(
    pool: &PgPool,
    user: Option<i64>,
    private: bool,
    since: i64,
    offset: i64,
    limit: i64,
) -> Result<Vec<Organization>> {
    Ok(sqlx::query_as("SELECT o.* FROM organizations o WHERE NOT o.is_stub AND o.id>$1 AND EXISTS(SELECT 1 FROM org_memberships own JOIN users u ON u.id=own.user_id WHERE own.org_id=o.id AND own.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) AND ($2::bigint IS NULL OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=o.id AND m.user_id=$2 AND m.role IN ('owner','admin','member') AND ($3 OR m.is_public))) ORDER BY o.id LIMIT $4 OFFSET $5")
        .bind(since).bind(user).bind(private).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn organization_public_repositories(pool: &PgPool, org: i64) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM repositories WHERE NOT is_stub AND owner_kind='org' AND owner_org_id=$1 AND NOT is_private")
        .bind(org).fetch_one(pool).await?)
}

pub async fn list_visible_members(
    pool: &PgPool,
    actor: Option<i64>,
    org: i64,
    public: bool,
    role: &str,
    filter: &str,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<User>>> {
    let mut tx = begin_read(pool).await?;
    let actor_role = if let Some(actor) = actor {
        match read_authority(&mut tx, actor, org).await? {
            Applied(role) => Some(role),
            _ => None,
        }
    } else {
        None
    };
    let member = actor_role.is_some();
    if filter != "all" && actor_role != Some(OrgRole::Owner) {
        return Ok(Denied);
    }
    let rows = sqlx::query_as("SELECT u.* FROM users u JOIN org_memberships m ON m.user_id=u.id WHERE m.org_id=$1 AND NOT u.is_stub AND m.role IN ('owner','admin','member') AND (($2 AND NOT $3) OR m.is_public) AND ($4='all' OR ($4='admin' AND m.role='owner') OR ($4='member' AND m.role IN ('admin','member'))) AND ($5='all' OR ($5='2fa_disabled' AND NOT u.two_factor_enabled)) ORDER BY u.username,u.id LIMIT $6 OFFSET $7")
        .bind(org).bind(member).bind(public).bind(role).bind(filter).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Applied(rows))
}

pub async fn list_membership_states(
    pool: &PgPool,
    actor: i64,
    org: Option<i64>,
    target: i64,
    state: Option<&str>,
    window: (i64, i64),
) -> Result<Vec<MembershipState>> {
    Ok(sqlx::query_as("SELECT m.org_id,m.user_id,m.role,m.state FROM (SELECT org_id,user_id,role,'active' AS state FROM org_memberships UNION ALL SELECT org_id,user_id,role,'pending' AS state FROM (SELECT i.org_id,u.id AS user_id,i.role,ROW_NUMBER() OVER(PARTITION BY i.org_id ORDER BY i.id) AS ordinal FROM organization_invitations i JOIN users u ON u.id=$1 AND ((i.email IS NULL AND i.user_id=u.id) OR (u.email_verified AND lower(u.email)=i.email) OR EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=u.id AND e.verified AND lower(e.email)=i.email)) WHERE i.failed_at IS NULL AND i.expires_at>NOW() AND ($2::bigint IS NULL OR i.org_id=$2) AND NOT EXISTS(SELECT 1 FROM org_memberships active WHERE active.org_id=i.org_id AND active.user_id=u.id)) pending WHERE ordinal=1) m WHERE m.role IN ('owner','admin','member') AND EXISTS(SELECT 1 FROM org_memberships own JOIN users u ON u.id=own.user_id WHERE own.org_id=m.org_id AND own.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) AND m.user_id=$1 AND ($2::bigint IS NULL OR m.org_id=$2) AND ($3::text IS NULL OR m.state=$3) AND (m.user_id=$4 OR EXISTS(SELECT 1 FROM org_memberships a JOIN users u ON u.id=a.user_id WHERE a.org_id=m.org_id AND a.user_id=$4 AND a.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password)) ORDER BY m.org_id LIMIT $5 OFFSET $6")
        .bind(target).bind(org).bind(state).bind(actor).bind(window.1).bind(window.0).fetch_all(pool).await?)
}

pub async fn set_membership_public(
    pool: &PgPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    public: bool,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    require_org!(tx, actor, org);
    if !credential(&mut tx, &grant, org, "members").await? {
        return Ok(Denied);
    }
    sqlx::query("UPDATE org_memberships SET is_public=$3 WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(actor)
        .bind(public)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(()))
}
