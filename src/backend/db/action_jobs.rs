//! Actual runner executions and steps. Private scheduler plans are not exposed as executions.
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_actions::progress::{Annotation, Event, State};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use sqlx::{types::Json, FromRow};

#[derive(FromRow)]
pub struct Job {
    pub id: i64,
    pub check_run_id: i64,
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
pub struct Summary {
    pub job: String,
    pub number: i32,
    pub markdown: String,
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

native_operation!(crate::backend::db::postgres::action_jobs::elapsed,crate::backend::db::sqlite::action_jobs::elapsed,
    fn elapsed(pool:&DbPool,repo:i64,run:i64)->Result<i64>);

pub struct Page {
    pub total: i64,
    pub jobs: Vec<Job>,
}

pub(crate) fn prepare_annotation(annotation: &Annotation) -> Result<Annotation> {
    let mut annotation = annotation.clone();
    annotation.path = annotation.path.replace('\0', "\\0");
    annotation.message = annotation.message.replace('\0', "\\0");
    annotation.title = annotation.title.map(|title| title.replace('\0', "\\0"));
    anyhow::ensure!(
        annotation.start_line > 0
            && annotation.end_line >= annotation.start_line
            && match (annotation.start_column, annotation.end_column) {
                (None, None) => true,
                (Some(start), Some(end)) =>
                    annotation.start_line == annotation.end_line && start > 0 && end >= start,
                _ => false,
            }
            && matches!(
                annotation.annotation_level.as_str(),
                "failure" | "warning" | "notice"
            )
            && serde_json::to_vec(&annotation)?.len() <= 64 * 1024,
        "invalid job annotation"
    );
    Ok(annotation)
}

pub(crate) fn validate_observed_state(
    state: &State,
    known: &[(String, i32, String)],
) -> Result<()> {
    let mut jobs = std::collections::HashMap::new();
    let mut variants = std::collections::HashMap::with_capacity(known.len());
    for (job, variant, status) in known {
        *jobs.entry(job.as_str()).or_insert(true) &= status == "completed";
        variants.insert((job.as_str(), *variant), status == "completed");
    }
    anyhow::ensure!(
        state
            .jobs
            .keys()
            .all(|job| jobs.get(job.as_str()) == Some(&true)),
        "state refers to an unobserved or unfinished job"
    );
    anyhow::ensure!(
        state
            .variants
            .iter()
            .all(
                |(job, saved)| saved.keys().all(|variant| i32::try_from(*variant)
                    .ok()
                    .is_some_and(|variant| variants.get(&(job.as_str(), variant)) == Some(&true)))
            ),
        "state refers to an unobserved or unfinished variant"
    );
    anyhow::ensure!(
        state
            .jobs
            .values()
            .chain(
                state
                    .variants
                    .values()
                    .flat_map(|variants| variants.values())
            )
            .all(|job| matches!(
                job.result.as_str(),
                "success" | "failure" | "cancelled" | "skipped"
            )),
        "invalid job result"
    );
    Ok(())
}

/// The job identity and whether this transaction owns a new lifecycle transition.
#[derive(Debug)]
pub struct Applied {
    pub id: i64,
    pub transitioned: bool,
}
native_operation!(crate::backend::db::postgres::action_jobs::apply,crate::backend::db::sqlite::action_jobs::apply,
    fn apply(pool:&DbPool,worker:&str,run:i64,generation:&str,workflow:&str,event:&Event)->Result<Applied>);
native_operation!(crate::backend::db::postgres::action_jobs::admit,crate::backend::db::sqlite::action_jobs::admit,
    fn admit(pool:&DbPool,worker:&str,run:i64,generation:&str,workflow:&str,job:i64)->Result<Option<bool>>);
native_operation!(crate::backend::db::postgres::action_jobs::save_state,crate::backend::db::sqlite::action_jobs::save_state,
    fn save_state(pool:&DbPool,worker:&str,run:i64,generation:&str,workflow:&str,state:&State)->Result<()>);
native_operation!(crate::backend::db::postgres::action_jobs::get,crate::backend::db::sqlite::action_jobs::get,
    /// A job the API shows: one that started, or with `queued` also one still waiting.
    fn get(pool:&DbPool,repo:i64,id:i64,queued:bool)->Result<Option<Job>>);
native_operation!(crate::backend::db::postgres::action_jobs::by_check_run,crate::backend::db::sqlite::action_jobs::by_check_run,
    fn by_check_run(pool:&DbPool,repo:i64,id:i64)->Result<Option<Job>>);
native_operation!(crate::backend::db::postgres::action_jobs::list,crate::backend::db::sqlite::action_jobs::list,
    fn list(pool:&DbPool,repo:i64,run:i64,all:bool,offset:i64,limit:i64)->Result<Page>);
native_operation!(crate::backend::db::postgres::action_jobs::steps,crate::backend::db::sqlite::action_jobs::steps,
    fn steps(pool:&DbPool,repo:i64,job:i64)->Result<Vec<Step>>);
native_operation!(crate::backend::db::postgres::action_jobs::summaries,crate::backend::db::sqlite::action_jobs::summaries,
    fn summaries(pool:&DbPool,repo:i64,run:i64)->Result<Vec<Summary>>);
native_operation!(crate::backend::db::postgres::action_jobs::annotations,crate::backend::db::sqlite::action_jobs::annotations,
    fn annotations(pool:&DbPool,repo:i64,job:i64,offset:i64,limit:i64)->Result<(i64,Vec<Json<Annotation>>)>);
native_operation!(crate::backend::db::postgres::action_jobs::observed,crate::backend::db::sqlite::action_jobs::observed,
    fn observed(pool:&DbPool,repo:i64,run:i64)->Result<bool>);
