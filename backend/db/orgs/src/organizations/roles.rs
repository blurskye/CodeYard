use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_database::models::Team;
use codeyard_database::models::User;
use codeyard_database::native_operation;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::DbPool;

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
    /// A team a native revoke form showed; a rename or replacement does not redirect it.
    TeamId(i64),
}

// Read projection only. Security mutations must pin tenant, account and membership authority.
native_operation!(crate::postgres::organizations::roles::can_manage_security,crate::sqlite::organizations::roles::can_manage_security,
    fn can_manage_security(pool:&DbPool,actor:i64,org:i64)->Result<bool>);

native_operation!(crate::postgres::organizations::roles::catalog,crate::sqlite::organizations::roles::catalog,
    fn catalog(pool:&DbPool,actor:i64,org:i64)->Result<OrganizationOutcome<Vec<OrganizationRole>>>);
native_operation!(crate::postgres::organizations::roles::users,crate::sqlite::organizations::roles::users,
    fn users(pool:&DbPool,actor:i64,org:i64,role:i64,after:i64,window:(i64,i64))->Result<OrganizationOutcome<Vec<UserAssignment>>>);
native_operation!(crate::postgres::organizations::roles::teams,crate::sqlite::organizations::roles::teams,
    fn teams(pool:&DbPool,actor:i64,org:i64,role:i64,after:i64,window:(i64,i64))->Result<OrganizationOutcome<Vec<Team>>>);
native_operation!(crate::postgres::organizations::roles::change,crate::sqlite::organizations::roles::change,
    fn change(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,org:i64,target:&RoleTarget,role:Option<i64>,assigned:bool)->Result<OrganizationOutcome<()>>);
