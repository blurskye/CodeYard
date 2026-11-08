use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
use codeyard_types::security_reports::NewReport;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Report {
    pub id: i64,
    pub repo_id: i64,
    pub reporter_id: Option<i64>,
    pub title: String,
    pub body: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub reporter: Option<String>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Reply {
    pub id: i64,
    pub report_id: i64,
    pub author_id: Option<i64>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub author: Option<String>,
}
pub struct Thread {
    pub report: Report,
    pub replies: Vec<Reply>,
}

native_operation!(crate::postgres::repositories::security::enabled,crate::sqlite::repositories::security::enabled,
    fn enabled(pool:&DbPool,repo_id:i64)->Result<bool>);
native_operation!(crate::postgres::repositories::security::set_enabled,crate::sqlite::repositories::security::set_enabled,
    fn set_enabled(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo_id:i64,enabled:bool)->Result<()>);
native_operation!(crate::postgres::repositories::security::create_report,crate::sqlite::repositories::security::create_report,
    fn create_report(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo_id:i64,input:&NewReport<'_>)->Result<i64>);
native_operation!(crate::postgres::repositories::security::list_reports,crate::sqlite::repositories::security::list_reports,
    fn list_reports(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo_id:i64,limit:i64,offset:i64)->Result<Vec<Report>>);
native_operation!(crate::postgres::repositories::security::report_thread,crate::sqlite::repositories::security::report_thread,
    fn report_thread(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo_id:i64,report_id:i64,offset:i64)->Result<Thread>);
native_operation!(crate::postgres::repositories::security::reply,crate::sqlite::repositories::security::reply,
    /// Returns the new reply's zero-based position in the oldest-first thread.
    fn reply(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo_id:i64,report_id:i64,body:&str)->Result<i64>);
native_operation!(crate::postgres::repositories::security::set_state,crate::sqlite::repositories::security::set_state,
    fn set_state(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,repo_id:i64,report_id:i64,closed:bool)->Result<()>);
