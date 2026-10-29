//! Actual runner executions and steps. Private scheduler plans are not exposed as executions.
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_actions::progress::{Annotation, Event, State};
use sqlx::{types::Json, FromRow};

#[derive(FromRow)]
pub struct Job {
    pub id: i64,
    pub run_id: i64,
    pub generation: String,
    pub workflow_path: String,
    pub workflow_name: Option<String>,
    pub matrix: Option<Json<serde_json::Value>>,
    pub logical_job: String,
    pub variant: i32,
    pub name: String,
    pub labels: Json<Vec<String>>,
    pub status: String,
    pub conclusion: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
}
#[derive(FromRow)]
pub struct Step {
    pub number: i32,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
}
#[derive(FromRow)]
pub struct Usage {
    pub id: i64,
    pub run_id: i64,
    pub labels: Json<Vec<String>>,
    pub duration_ms: i64,
    pub run_started_at: Option<DateTime<Utc>>,
    pub run_completed_at: Option<DateTime<Utc>>,
}

native_operation!(crate::backend::db::postgres::action_jobs::usage,crate::backend::db::sqlite::action_jobs::usage,
    fn usage(pool:&DbPool,repo:i64,run:Option<i64>,workflow:Option<&str>,since:DateTime<Utc>)->Result<Vec<Usage>>);

pub struct Page {
    pub total: i64,
    pub jobs: Vec<Job>,
}

pub(crate) fn validate_annotation(annotation: &Annotation) -> Result<()> {
    anyhow::ensure!(
        annotation.start_line > 0
            && annotation.end_line >= annotation.start_line
            && match (annotation.start_column, annotation.end_column) {
                (None, None) => true,
                (Some(start), Some(end)) => annotation.start_line == annotation.end_line && start > 0 && end >= start,
                _ => false,
            }
            && matches!(
                annotation.annotation_level.as_str(),
                "failure" | "warning" | "notice"
            )
            && serde_json::to_vec(annotation)?.len() <= 64 * 1024,
        "invalid job annotation"
    );
    Ok(())
}

native_operation!(crate::backend::db::postgres::action_jobs::apply,crate::backend::db::sqlite::action_jobs::apply,
    fn apply(pool:&DbPool,worker:&str,run:i64,generation:&str,workflow:&str,event:&Event)->Result<i64>);
native_operation!(crate::backend::db::postgres::action_jobs::admit,crate::backend::db::sqlite::action_jobs::admit,
    fn admit(pool:&DbPool,worker:&str,run:i64,generation:&str,workflow:&str,job:i64)->Result<Option<bool>>);
native_operation!(crate::backend::db::postgres::action_jobs::save_state,crate::backend::db::sqlite::action_jobs::save_state,
    fn save_state(pool:&DbPool,worker:&str,run:i64,generation:&str,workflow:&str,state:&State)->Result<()>);
native_operation!(crate::backend::db::postgres::action_jobs::get,crate::backend::db::sqlite::action_jobs::get,
    /// A job the API shows: one that started, or with `queued` also one still waiting.
    fn get(pool:&DbPool,repo:i64,id:i64,queued:bool)->Result<Option<Job>>);
native_operation!(crate::backend::db::postgres::action_jobs::list,crate::backend::db::sqlite::action_jobs::list,
    fn list(pool:&DbPool,repo:i64,run:i64,all:bool,offset:i64,limit:i64)->Result<Page>);
native_operation!(crate::backend::db::postgres::action_jobs::steps,crate::backend::db::sqlite::action_jobs::steps,
    fn steps(pool:&DbPool,repo:i64,job:i64)->Result<Vec<Step>>);
native_operation!(crate::backend::db::postgres::action_jobs::annotations,crate::backend::db::sqlite::action_jobs::annotations,
    fn annotations(pool:&DbPool,repo:i64,job:i64)->Result<Vec<Json<Annotation>>>);
native_operation!(crate::backend::db::postgres::action_jobs::observed,crate::backend::db::sqlite::action_jobs::observed,
    fn observed(pool:&DbPool,repo:i64,run:i64)->Result<bool>);
