use super::authority::*;
use crate::backend::db::{
    models::{OrgRole, Organization},
    organizations::{
        settings::{Changes, Settings, Updated},
        OrganizationOutcome,
    },
};
use anyhow::{Context, Result};
use sqlx::{SqliteConnection, SqlitePool};
use OrganizationOutcome::{Applied, Denied};

pub(in crate::backend::db::sqlite) async fn get_on(
    c: &mut SqliteConnection,
    org: i64,
) -> Result<Option<Settings>> {
    Ok(sqlx::query_as("SELECT o.id AS org_id,s.company,s.email,s.twitter_username,s.location,s.blog,COALESCE(s.has_organization_projects,1) AS has_organization_projects,COALESCE(s.default_repository_permission,0) AS default_repository_permission,COALESCE(s.members_can_create_public_repositories,0) AS members_can_create_public_repositories,COALESCE(s.members_can_create_private_repositories,0) AS members_can_create_private_repositories,COALESCE(s.members_can_fork_private_repositories,1) AS members_can_fork_private_repositories,COALESCE(s.web_commit_signoff_required,0) AS web_commit_signoff_required,COALESCE(s.deploy_keys_enabled_for_repositories,1) AS deploy_keys_enabled_for_repositories FROM organizations o LEFT JOIN organization_settings s ON s.org_id=o.id WHERE o.id=?1").bind(org).fetch_optional(c).await?)
}
pub async fn get(pool: &SqlitePool, org: i64) -> Result<Option<Settings>> {
    let mut c = pool.acquire().await?;
    get_on(&mut c, org).await
}
pub async fn repository_requires_web_signoff(pool: &SqlitePool, repo: i64) -> Result<Option<bool>> {
    Ok(sqlx::query_scalar("SELECT COALESCE(s.web_commit_signoff_required,FALSE) FROM repositories r LEFT JOIN organization_settings s ON r.owner_kind='org' AND s.org_id=r.owner_org_id WHERE r.id=$1").bind(repo).fetch_optional(pool).await?)
}

pub async fn owned(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Updated>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let settings = get_on(&mut tx, org)
        .await?
        .context("Organization settings scope disappeared")?;
    let organization = sqlx::query_as::<_, Organization>("SELECT * FROM organizations WHERE id=?1")
        .bind(org)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(Updated {
        organization,
        settings,
    }))
}
pub async fn change(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::users::tokens::Actor>,
    org: i64,
    input: &Changes,
) -> Result<OrganizationOutcome<Updated>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !credential(&mut tx, &grant, org, "organization_administration").await? { return Ok(Denied); }
    let mut settings = get_on(&mut tx, org)
        .await?
        .context("Organization settings scope disappeared")?;
    let previous = settings.default_repository_permission;
    input.apply(&mut settings);
    sqlx::query("INSERT INTO organization_settings(org_id,company,email,twitter_username,location,blog,has_organization_projects,default_repository_permission,members_can_create_public_repositories,members_can_create_private_repositories,members_can_fork_private_repositories,web_commit_signoff_required,deploy_keys_enabled_for_repositories) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(org_id) DO UPDATE SET company=excluded.company,email=excluded.email,twitter_username=excluded.twitter_username,location=excluded.location,blog=excluded.blog,has_organization_projects=excluded.has_organization_projects,default_repository_permission=excluded.default_repository_permission,members_can_create_public_repositories=excluded.members_can_create_public_repositories,members_can_create_private_repositories=excluded.members_can_create_private_repositories,members_can_fork_private_repositories=excluded.members_can_fork_private_repositories,web_commit_signoff_required=excluded.web_commit_signoff_required,deploy_keys_enabled_for_repositories=excluded.deploy_keys_enabled_for_repositories")
        .bind(org).bind(&settings.company).bind(&settings.email).bind(&settings.twitter_username).bind(&settings.location).bind(&settings.blog).bind(settings.has_organization_projects).bind(settings.default_repository_permission).bind(settings.members_can_create_public_repositories).bind(settings.members_can_create_private_repositories).bind(settings.members_can_fork_private_repositories).bind(settings.web_commit_signoff_required).bind(settings.deploy_keys_enabled_for_repositories).execute(&mut *tx).await?;
    let organization=sqlx::query_as::<_,Organization>("UPDATE organizations SET display_name=CASE WHEN ?2 THEN ?3 ELSE display_name END,description=CASE WHEN ?4 THEN ?5 ELSE description END,billing_email=CASE WHEN ?6 THEN ?7 ELSE billing_email END,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 RETURNING *")
        .bind(org).bind(input.profile.display_name.is_some()).bind(input.profile.display_name.clone().flatten()).bind(input.profile.description.is_some()).bind(input.profile.description.clone().flatten()).bind(input.billing_email.is_some()).bind(input.billing_email.clone().flatten()).fetch_one(&mut *tx).await?;
    if previous >= 3 && settings.default_repository_permission < 3 {
        let access = super::super::repositories::access::USER_REPO_ACCESS_SQL;
        let runs:Vec<i64>=sqlx::query_scalar(&format!("SELECT c.id FROM ci_runs c JOIN repositories r ON r.id=c.repo_id JOIN users u ON u.id=c.triggered_by_id JOIN org_memberships m ON m.org_id=r.owner_org_id AND m.user_id=u.id AND m.role='member' WHERE r.owner_kind='org' AND r.owner_org_id=?1 AND r.owner_user_id IS NULL AND (c.status IN ('queued','running') OR c.completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) AND ({access})<3 ORDER BY c.id")).bind(org).fetch_all(&mut *tx).await?;
        super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
    }
    tx.commit().await?;
    Ok(Applied(Updated {
        organization,
        settings,
    }))
}
