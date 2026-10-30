//! Predefined organization roles grant repository or security authority, never organization ownership.
use super::OrganizationOutcome;
use crate::backend::db::{
    models::{Team, User},
    DbPool,
};
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Shared by the legacy security-team API and the organization-role API.
pub const SECURITY_MANAGER_ROLE_ID: i64 = 6;

/// Read projection shared by feature listings and locked security administration.
pub(crate) fn security_manager_condition(actor: &str, org: &str) -> String {
    format!("EXISTS(SELECT 1 FROM organizations security_org JOIN org_memberships security_member ON security_member.org_id=security_org.id JOIN users security_user ON security_user.id=security_member.user_id WHERE security_org.id={org} AND security_user.id={actor} AND security_member.role IN ('owner','admin','member') AND NOT security_user.is_banned AND NOT security_user.must_change_password AND NOT EXISTS(SELECT 1 FROM organization_blocks blocked WHERE blocked.org_id=security_org.id AND blocked.user_id=security_user.id) AND EXISTS(SELECT 1 FROM org_memberships owner JOIN users active ON active.id=owner.user_id WHERE owner.org_id=security_org.id AND owner.role='owner' AND NOT active.is_banned AND NOT active.must_change_password) AND (security_member.role='owner' OR EXISTS(SELECT 1 FROM organization_role_assignments assignment JOIN organization_roles role ON role.id=assignment.role_id AND role.base_level=1 WHERE assignment.org_id=security_org.id AND assignment.role_id={SECURITY_MANAGER_ROLE_ID} AND ((assignment.user_id=security_user.id AND assignment.team_id IS NULL) OR (assignment.user_id IS NULL AND EXISTS(SELECT 1 FROM teams security_team JOIN team_ancestors lineage ON lineage.ancestor_id=security_team.id JOIN team_memberships team_member ON team_member.team_id=lineage.team_id WHERE security_team.id=assignment.team_id AND security_team.org_id=assignment.org_id AND security_team.privacy IN ('visible','secret') AND team_member.user_id=security_user.id AND team_member.role IN ('member','maintainer')))))))")
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct OrganizationRole {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub base_level: i16,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Debug, sqlx::FromRow)]
pub struct UserAssignment {
    #[sqlx(flatten)]
    pub user: User,
    pub direct: bool,
    #[sqlx(skip)]
    pub inherited_from: Vec<Team>,
}
#[derive(sqlx::FromRow)]
pub(crate) struct InheritedTeam {
    pub user_id: i64,
    #[sqlx(flatten)]
    pub team: Team,
}

#[derive(Clone, Debug)]
pub enum RoleTarget {
    User(String),
    Team(String),
}

// Read projection only. Security mutations must pin tenant, account and membership authority.
native_operation!(crate::backend::db::postgres::organizations::roles::can_manage_security,crate::backend::db::sqlite::organizations::roles::can_manage_security,
    fn can_manage_security(pool:&DbPool,actor:i64,org:i64)->Result<bool>);

native_operation!(crate::backend::db::postgres::organizations::roles::catalog,crate::backend::db::sqlite::organizations::roles::catalog,
    fn catalog(pool:&DbPool,actor:i64,org:i64)->Result<OrganizationOutcome<Vec<OrganizationRole>>>);
native_operation!(crate::backend::db::postgres::organizations::roles::users,crate::backend::db::sqlite::organizations::roles::users,
    fn users(pool:&DbPool,actor:i64,org:i64,role:i64,after:i64,window:(i64,i64))->Result<OrganizationOutcome<Vec<UserAssignment>>>);
native_operation!(crate::backend::db::postgres::organizations::roles::teams,crate::backend::db::sqlite::organizations::roles::teams,
    fn teams(pool:&DbPool,actor:i64,org:i64,role:i64,after:i64,window:(i64,i64))->Result<OrganizationOutcome<Vec<Team>>>);
native_operation!(crate::backend::db::postgres::organizations::roles::change,crate::backend::db::sqlite::organizations::roles::change,
    fn change(pool:&DbPool,actor:impl Into<crate::backend::db::credentials::Actor>,org:i64,target:&RoleTarget,role:Option<i64>,assigned:bool)->Result<OrganizationOutcome<()>>);
