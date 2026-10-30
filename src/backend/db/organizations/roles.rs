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
    fn change(pool:&DbPool,actor:impl Into<crate::backend::db::users::tokens::Actor>,org:i64,target:&RoleTarget,role:Option<i64>,assigned:bool)->Result<OrganizationOutcome<()>>);
