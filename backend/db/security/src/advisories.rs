use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
use codeyard_types::advisories::Advisory;
use serde_json::Value;

#[derive(Clone, Copy)]
pub enum Scope {
    Repository(i64),
    Organization(i64),
}
#[derive(Clone, Copy, Default)]
pub enum Sort {
    #[default]
    Created,
    Updated,
    Published,
}
impl Sort {
    pub fn timestamp(&self, row: &Advisory) -> DateTime<Utc> {
        match self {
            Self::Created => row.created_at,
            Self::Updated => row.updated_at,
            Self::Published => row.published_at.unwrap_or(row.created_at),
        }
    }
    pub fn column(&self) -> &'static str {
        match self {
            Self::Created => "a.created_at",
            Self::Updated => "a.updated_at",
            Self::Published => "COALESCE(a.published_at,a.created_at)",
        }
    }
}
#[derive(Default)]
pub struct Listing<'a> {
    pub state: Option<&'a str>,
    pub sort: Sort,
    pub ascending: bool,
    pub before: bool,
    pub cursor: Option<(DateTime<Utc>, i64)>,
    pub offset: i64,
    pub limit: i64,
}

/// A UI/preflight projection, never a mutation permit.
pub(crate) fn administration_sql(level: &str) -> String {
    let manager = codeyard_database::queries::organizations::roles::security_manager_condition(
        "u.id",
        "r.owner_org_id",
    );
    format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2 AND NOT r.is_gist AND NOT r.is_archived AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM repository_migration_locks l WHERE l.repo_id=r.id) AND ((r.owner_kind='user' AND (r.owner_user_id=u.id OR u.is_admin)) OR ({level})>=5 OR (r.owner_kind='org' AND {manager})))")
}

native_operation!(crate::postgres::advisories::can_administer,crate::sqlite::advisories::can_administer,
    fn can_administer(pool:&DbPool,actor:i64,repo:i64)->Result<bool>);
native_operation!(crate::postgres::advisories::list,crate::sqlite::advisories::list,
    fn list(pool:&DbPool,actor:Option<i64>,scope:Scope,options:&Listing<'_>)->Result<Vec<Advisory>>);
native_operation!(crate::postgres::advisories::get,crate::sqlite::advisories::get,
    fn get(pool:&DbPool,actor:Option<i64>,repo:i64,ghsa:&str)->Result<Option<Advisory>>);
native_operation!(crate::postgres::advisories::create,crate::sqlite::advisories::create,
    fn create(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo:i64,value:&Value,report:bool)->Result<Advisory>);
native_operation!(crate::postgres::advisories::discard_created,crate::sqlite::advisories::discard_created,
    fn discard_created(pool:&DbPool,actor:i64,created:&Advisory)->Result<bool>);
native_operation!(crate::postgres::advisories::update,crate::sqlite::advisories::update,
    fn update(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo:i64,ghsa:&str,patch:&Value)->Result<Advisory>);
native_operation!(crate::postgres::advisories::request_cve,crate::sqlite::advisories::request_cve,
    fn request_cve(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo:i64,ghsa:&str)->Result<()>);
native_operation!(crate::postgres::advisories::attach_fork,crate::sqlite::advisories::attach_fork,
    fn attach_fork(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo:i64,ghsa:&str,fork:i64)->Result<()>);
