//! Durable workflow-run identities and repository-scoped history.
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_db_core::models::CiRun;
use codeyard_db_core::models::Repository;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use sqlx::{types::Json, FromRow};

#[derive(FromRow)]
pub struct RunMetadata {
    pub run_id: i64,
    pub workflow_id: i64,
    #[sqlx(default)]
    pub origin_created_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub actor_id: Option<i64>,
    // Internal snapshot: never serialize this record into a response. Repository payloads
    // select public fields and keep storage placement/paths out of HTTP documents.
    pub head_repository: Option<Json<Repository>>,
    pub head_owner: Option<String>,
    /// The run's check suite; read with the metadata, not stored with it.
    #[sqlx(default)]
    pub check_suite_id: Option<i64>,
}

#[derive(Default)]
pub struct RunQuery {
    pub workflow: String,
    pub actor: String,
    pub branch: String,
    pub event: String,
    pub statuses: Vec<String>,
    pub completed: bool,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub since_exclusive: bool,
    pub until_inclusive: bool,
    pub sha: String,
    pub check_suite: Option<i64>,
}
pub struct RunPage {
    pub total: i64,
    pub runs: Vec<CiRun>,
}

// A native terminal outcome can be less specific than a GitHub conclusion. Unknown
// historical values stay unknown instead of manufacturing a successful completion.
// `waiting` is never stored: list filters read a queued run held for approval as waiting.
const STATES: &[(&str, &str, Option<&str>)] = &[
    ("waiting", "waiting", None),
    ("queued", "queued", None),
    ("pending", "queued", None),
    ("running", "in_progress", None),
    ("in_progress", "in_progress", None),
    ("success", "completed", Some("success")),
    ("completed", "completed", None),
    ("failed", "completed", Some("failure")),
    ("error", "completed", Some("failure")),
    ("cancelled", "completed", Some("cancelled")),
    ("canceled", "completed", Some("cancelled")),
    ("timed_out", "completed", Some("timed_out")),
    ("skipped", "completed", Some("skipped")),
];
pub fn state(value: &str) -> (&'static str, Option<&'static str>) {
    STATES
        .iter()
        .find(|(native, _, _)| *native == value)
        .map(|(_, status, conclusion)| (*status, *conclusion))
        .unwrap_or(("completed", None))
}
pub(crate) fn state_sql(pg: bool, run: &str) -> String {
    let protection = crate::backend::db::deployment_protection::waiting_sql(pg, run);
    format!("CASE WHEN {run}.status='queued' AND ({run}.approval_required OR {protection}) THEN 'waiting' ELSE {run}.status END")
}

/// Fork approval and pending environment protection share the REST waiting phase.
pub fn run_state(run: &CiRun) -> (&'static str, Option<&'static str>) {
    state_with_approval(&run.status, run.approval_required || run.deployment_waiting)
}

/// The REST state of a run with `status`; `waiting` says a fork approval or an environment
/// protection holds it.
pub fn state_with_approval(status: &str, waiting: bool) -> (&'static str, Option<&'static str>) {
    if waiting && status == "queued" {
        ("waiting", None)
    } else {
        state(status)
    }
}
pub fn native_statuses(value: &str) -> Vec<String> {
    STATES
        .iter()
        .filter(|(_, status, conclusion)| *status == value || *conclusion == Some(value))
        .map(|(native, _, _)| (*native).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_phase_is_not_a_successful_outcome() {
        assert_eq!(state("success"), ("completed", Some("success")));
        assert_eq!(state("completed"), ("completed", None));
        assert_eq!(state("unknown_legacy_outcome"), ("completed", None));
        assert!(!native_statuses("success").contains(&"completed".to_owned()));
    }
}

native_operation!(crate::backend::db::postgres::action_runs::get,crate::backend::db::sqlite::action_runs::get,
    fn get(pool:&DbPool,repo:i64,id:i64,attempt:Option<i32>) -> Result<Option<CiRun>>);
native_operation!(crate::backend::db::postgres::action_runs::metadata,crate::backend::db::sqlite::action_runs::metadata,
    fn metadata(pool:&DbPool,repo:i64,ids:&[i64]) -> Result<Vec<RunMetadata>>);
native_operation!(crate::backend::db::postgres::action_runs::list,crate::backend::db::sqlite::action_runs::list,
    fn list(pool:&DbPool,actor:Option<i64>,repo:i64,query:&RunQuery,offset:i64,limit:i64) -> Result<Option<RunPage>>);
