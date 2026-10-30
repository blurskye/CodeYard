//! Private security reporting. Reports are never ordinary issues or searchable public activity.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Private security reporting denied")]
    Denied,
    #[error("Security report not found")]
    Missing,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Security reporting limit reached")]
    Limit,
}
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
pub struct NewReport<'a> {
    pub(in crate::backend::db) title: &'a str,
    pub(in crate::backend::db) body: &'a str,
}
impl<'a> NewReport<'a> {
    pub fn new(title: &'a str, body: &'a str) -> std::result::Result<Self, Refusal> {
        if title.trim().is_empty() || title.len() > 255 || title.chars().any(char::is_control) {
            return Err(Refusal::Invalid(
                "A report needs a single-line title of at most 255 bytes",
            ));
        }
        validate_body(body)?;
        Ok(Self { title, body })
    }
}
pub fn validate_body(body: &str) -> std::result::Result<(), Refusal> {
    if body.trim().is_empty() || body.len() > 65536 || body.contains('\0') {
        return Err(Refusal::Invalid(
            "A report or reply needs text of at most 65536 bytes",
        ));
    }
    Ok(())
}

native_operation!(crate::backend::db::postgres::repositories::security::enabled,crate::backend::db::sqlite::repositories::security::enabled,
    fn enabled(pool:&DbPool,repo_id:i64)->Result<bool>);
native_operation!(crate::backend::db::postgres::repositories::security::set_enabled,crate::backend::db::sqlite::repositories::security::set_enabled,
    fn set_enabled(pool:&DbPool,actor:impl Into<crate::backend::db::users::tokens::Actor>,repo_id:i64,enabled:bool)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::security::create_report,crate::backend::db::sqlite::repositories::security::create_report,
    fn create_report(pool:&DbPool,actor:i64,repo_id:i64,input:&NewReport<'_>)->Result<i64>);
native_operation!(crate::backend::db::postgres::repositories::security::list_reports,crate::backend::db::sqlite::repositories::security::list_reports,
    fn list_reports(pool:&DbPool,actor:i64,session:&str,repo_id:i64,limit:i64,offset:i64)->Result<Vec<Report>>);
native_operation!(crate::backend::db::postgres::repositories::security::report_thread,crate::backend::db::sqlite::repositories::security::report_thread,
    fn report_thread(pool:&DbPool,actor:i64,session:&str,repo_id:i64,report_id:i64,offset:i64)->Result<Thread>);
native_operation!(crate::backend::db::postgres::repositories::security::reply,crate::backend::db::sqlite::repositories::security::reply,
    fn reply(pool:&DbPool,actor:i64,repo_id:i64,report_id:i64,body:&str)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::security::set_state,crate::backend::db::sqlite::repositories::security::set_state,
    fn set_state(pool:&DbPool,actor:i64,repo_id:i64,report_id:i64,closed:bool)->Result<()>);
