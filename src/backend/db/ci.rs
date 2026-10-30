//! CI runs, logs, deployments and the job queue that worker nodes poll.
use super::models::{CiDeployment, CiLogLine, CiRun};
use super::DbPool;
use anyhow::Result;
use uuid::Uuid;

/// Policy refusals from the native CI enqueue and cancellation transactions.
#[derive(Debug, thiserror::Error)]
pub enum CiAdmissionRefusal {
    #[error("Workflow is disabled")]
    WorkflowDisabled,
    #[error("{0}")]
    Missing(&'static str),
    #[error("{0}")]
    Denied(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
}

#[derive(Clone, Copy)]
pub enum CiRunDeletion {
    Run,
    Logs,
}

/// A pull request run repeats the latest one for its head and base when it describes the
/// same event. Every `synchronize` for one head is the same event: Codeyard also sends one
/// when an unchanged head moves to a new base.
pub(crate) fn repeats_pull_request_event(
    latest: Option<&serde_json::Value>,
    next: Option<&serde_json::Value>,
) -> bool {
    let Some(next) = next else {
        return true;
    };
    latest == Some(next)
        || (next["action"] == "synchronize"
            && latest.is_some_and(|latest| latest["action"] == "synchronize"))
}

pub(crate) fn may_cancel_run(
    actor_id: i64,
    triggered_by_id: Option<i64>,
    personal_admin: bool,
    access: super::models::AccessLevel,
) -> bool {
    triggered_by_id == Some(actor_id) || personal_admin || access.can_write()
}

pub struct NewCiRun<'a> {
    pub repo_id: i64,
    pub triggered_by_id: i64,
    pub pipeline: &'a str,
    pub event: &'a str,
    /// The pull request whose revision and workflow approval admit this run.
    pub pull_request_id: Option<i64>,
    pub branch: &'a str,
    /// The exact commit the workflows run against.
    pub head_sha: &'a str,
    pub workflow_path: &'a str,
    /// Workflow path → blob id at `head_sha`, so the run uses exactly those files.
    pub workflow_blobs: &'a serde_json::Value,
    /// Input name → value for a manual `workflow_dispatch` run, as GitHub's `event.inputs`.
    pub workflow_inputs: Option<&'a serde_json::Value>,
    pub environment: Option<&'a str>,
    /// Whether a person asked for the run, rather than an event. A pull request revision whose
    /// newest attempt has finished gets a new attempt for such a request. Automatic admission
    /// reuses that newest attempt, whatever its status.
    pub manual: bool,
}

/// A deliberate execution of a stored workflow snapshot, not a newly discovered revision.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CiReplaySelection {
    All,
    Failed,
    Job(i64),
}

pub struct NewCiAttempt {
    pub source: i64,
    pub source_uuid: Uuid,
    pub credential: Option<i64>,
    pub authority: Option<super::credentials::Actor>,
    /// Native forms retain their explicit duplicate/reuse behavior; REST attempts require idle roots.
    pub allow_active: bool,
    pub selection: CiReplaySelection,
    pub definition: Option<String>,
    pub debug: bool,
}

pub(crate) fn validate_ci_attempt(
    source: &CiRun,
    input: &NewCiRun<'_>,
    attempt: &NewCiAttempt,
) -> Result<()> {
    anyhow::ensure!(
        input.manual
            && (!attempt.allow_active || attempt.selection == CiReplaySelection::All)
            && source.pull_request_id == input.pull_request_id
            && source.run_id == attempt.source_uuid
            && source.repo_id == input.repo_id
            && source.workflow_path == input.workflow_path
            && source.branch == input.branch
            && source.head_sha.as_deref() == Some(input.head_sha)
            && source.event == input.event
            && source.environment.as_deref() == input.environment
            && source.pipeline == input.pipeline
            && &source.workflow_blobs == input.workflow_blobs
            && source.workflow_inputs.as_ref() == input.workflow_inputs,
        CiAdmissionRefusal::Conflict("Rerun source changed")
    );
    anyhow::ensure!(
        source.workflow_run_number.is_some_and(|number| number > 0),
        CiAdmissionRefusal::Conflict("Source workflow metadata is unavailable")
    );
    anyhow::ensure!(
        attempt.selection == CiReplaySelection::All
            || source.engine.as_deref() == Some("codeyard-actions"),
        CiAdmissionRefusal::Conflict(
            "Partial re-runs require native job observations and dependency outputs"
        )
    );
    Ok(())
}

#[derive(Default, sqlx::FromRow)]
pub struct CiRunOptions {
    pub replay: Option<sqlx::types::Json<codeyard_actions::progress::Replay>>,
    pub debug: bool,
}
native_operation!(crate::backend::db::postgres::ci::ci_run_options,crate::backend::db::sqlite::ci::ci_run_options,
    fn ci_run_options(pool:&DbPool,run:i64)->Result<Option<CiRunOptions>>);

/// Recorded on queued runs that are cancelled because the person who started them
/// lost access, and shown to the person who looks at the run.
pub const ACCESS_LOST_MESSAGE: &str = "The person who started this run no longer has access";

native_operation!(crate::backend::db::postgres::ci::scheduled_repositories,
    crate::backend::db::sqlite::ci::scheduled_repositories,
    fn scheduled_repositories(pool: &DbPool, after: i64) -> Result<Vec<i64>>);

// Commits retained by recorded runs, including runs whose branch no longer exists.
native_operation!(crate::backend::db::postgres::ci::retained_ci_commits,
    crate::backend::db::sqlite::ci::retained_ci_commits,
    fn retained_ci_commits(pool: &DbPool, repo_id: i64) -> Result<Vec<String>>);

native_operation!(crate::backend::db::postgres::ci::enqueue_ci_run,
    crate::backend::db::sqlite::ci::enqueue_ci_run,
    fn enqueue_ci_run(pool: &DbPool, input: NewCiRun<'_>, trusted_ids: &[i64]) -> Result<CiRun>);

native_operation!(crate::backend::db::postgres::ci::enqueue_ci_run_attempt,
    crate::backend::db::sqlite::ci::enqueue_ci_run_attempt,
    fn enqueue_ci_run_attempt(pool:&DbPool,input:NewCiRun<'_>,trusted_ids:&[i64],attempt:NewCiAttempt,scheduling:&super::runners::Scheduling,event_payload:Option<&serde_json::Value>)->Result<CiRun>);
native_operation!(crate::backend::db::postgres::ci::enqueue_ci_run_for_runners,
    crate::backend::db::sqlite::ci::enqueue_ci_run_for_runners,
    fn enqueue_ci_run_for_runners(pool: &DbPool, input: NewCiRun<'_>, trusted_ids: &[i64], scheduling: &super::runners::Scheduling, event_payload: Option<&serde_json::Value>) -> Result<CiRun>);

native_operation!(crate::backend::db::postgres::ci::claim_ci_run,
    crate::backend::db::sqlite::ci::claim_ci_run,
    /// Hands the oldest queued run to `worker`, after checking again that the person
    /// who started it may still run workflows here. Queued runs whose starter lost
    /// access are cancelled instead, and the claim moves on to the next one. A worker
    /// that already holds a running run gets that run back, so a claim retried after a
    /// lost reply does not leave its run stranded.
    fn claim_ci_run(pool: &DbPool, worker: &str, mode: &str, trusted_ids: &[i64], platforms: &[String]) -> Result<Option<CiRun>>);

native_operation!(crate::backend::db::postgres::ci::claim_ci_run_on_node,
    crate::backend::db::sqlite::ci::claim_ci_run_on_node,
    /// Like `claim_ci_run`, for a cluster node: the node must still be an active
    /// actions worker when the run is taken. The check runs inside the claim
    /// transaction and holds the node row until the claim commits, so a revocation
    /// waits for the claim or lands before it. Checking in an earlier query let a
    /// worker whose node was just revoked take a run and fail it instead of leaving
    /// it queued.
    fn claim_ci_run_on_node(
        pool: &DbPool,
        node_key: &str,
        worker: &str,
        mode: &str,
        trusted_ids: &[i64],
        platforms: &[String],
    ) -> Result<Option<CiRun>>);

native_operation!(crate::backend::db::postgres::ci::ci_run_allows_repository_read,
    crate::backend::db::sqlite::ci::ci_run_allows_repository_read,
    /// Whether this exact running claim may read the repository. Remote worker ids
    /// are the authenticated public key followed by `/` and the worker id.
    fn ci_run_allows_repository_read(
        pool: &DbPool,
        run_id: i64,
        repo_id: i64,
        worker: &str,
        public_key: &str,
    ) -> Result<bool>);

native_operation!(crate::backend::db::postgres::ci::heartbeat_ci_run,
    crate::backend::db::sqlite::ci::heartbeat_ci_run,
    fn heartbeat_ci_run(pool: &DbPool, id: i64, worker: &str) -> Result<Option<bool>>);

native_operation!(crate::backend::db::postgres::ci::finish_ci_run,
    crate::backend::db::sqlite::ci::finish_ci_run,
    fn finish_ci_run(
    pool: &DbPool,
    id: i64,
    worker: &str,
    status: &str,
    message: Option<&str>,
) -> Result<Option<(CiRun, Vec<(i64, i64)>)>>);

native_operation!(crate::backend::db::postgres::ci::fail_stale_ci_runs,
    crate::backend::db::sqlite::ci::fail_stale_ci_runs,
    fn fail_stale_ci_runs(pool: &DbPool, stale_seconds: i64) -> Result<(Vec<CiRun>, Vec<(i64, i64)>)>);

native_operation!(crate::backend::db::postgres::ci::request_ci_cancellation,
    crate::backend::db::sqlite::ci::request_ci_cancellation,
    fn request_ci_cancellation(pool: &DbPool, run_id: Uuid, actor_id: i64) -> Result<(bool, Vec<(i64, i64)>)>);

native_operation!(crate::backend::db::postgres::ci::request_ci_cancellation_with_token,
    crate::backend::db::sqlite::ci::request_ci_cancellation_with_token,
    fn request_ci_cancellation_with_token(pool: &DbPool, run_id: Uuid, actor_id: i64, token_id: i64) -> Result<(bool, Vec<(i64,i64)>)>);
native_operation!(crate::backend::db::postgres::ci::delete_ci_run_data,
    crate::backend::db::sqlite::ci::delete_ci_run_data,
    fn delete_ci_run_data(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, id: i64, kind: CiRunDeletion) -> Result<bool>);
native_operation!(crate::backend::db::postgres::ci::get_ci_run_by_id,
    crate::backend::db::sqlite::ci::get_ci_run_by_id,
    fn get_ci_run_by_id(pool: &DbPool, repo_id: i64, id: i64) -> Result<Option<CiRun>>);
native_operation!(crate::backend::db::postgres::ci::get_ci_run,
    crate::backend::db::sqlite::ci::get_ci_run,
    fn get_ci_run(pool: &DbPool, id: i64) -> Result<Option<CiRun>>);
native_operation!(crate::backend::db::postgres::ci::get_ci_run_by_run_id,
    crate::backend::db::sqlite::ci::get_ci_run_by_run_id,
    fn get_ci_run_by_run_id(pool: &DbPool, run_id: Uuid) -> Result<Option<CiRun>>);

/// REST filters use GitHub's status and conclusion names and inclusive creation bounds.
#[derive(Debug, Default)]
pub struct RestRunFilter {
    pub workflow: Option<String>,
    pub branch: Option<String>,
    pub actor: Option<String>,
    pub event: Option<String>,
    pub status: Option<String>,
    pub head_sha: Option<String>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

native_operation!(crate::backend::db::postgres::ci::list_rest_ci_runs,
    crate::backend::db::sqlite::ci::list_rest_ci_runs,
    fn list_rest_ci_runs(pool: &DbPool, repo_id: i64, filter: &RestRunFilter, limit: i64, offset: i64) -> Result<(i64, Vec<CiRun>)>);

native_operation!(crate::backend::db::postgres::ci::ci_workflow_paths,
    crate::backend::db::sqlite::ci::ci_workflow_paths,
    fn ci_workflow_paths(pool: &DbPool, repo_id: i64) -> Result<Vec<String>>);

#[derive(Debug, Default)]
pub struct CiRunFilter {
    pub workflow: String,
    pub event: String,
    pub branch: String,
    pub status: String,
    pub text: String,
    pub actor: String,
    pub before: Option<i64>,
}

native_operation!(crate::backend::db::postgres::ci::list_filtered_ci_runs,
    crate::backend::db::sqlite::ci::list_filtered_ci_runs,
    fn list_filtered_ci_runs(pool: &DbPool, actor: i64, repo: Option<i64>, filter: &CiRunFilter, limit: i64) -> Result<Vec<CiRun>>);

native_operation!(crate::backend::db::postgres::ci::list_ci_runs_for_repo,
    crate::backend::db::sqlite::ci::list_ci_runs_for_repo,
    fn list_ci_runs_for_repo(pool: &DbPool, repo_id: i64, limit: i64) -> Result<Vec<CiRun>>);

native_operation!(crate::backend::db::postgres::ci::latest_ci_runs_for_commit,
    crate::backend::db::sqlite::ci::latest_ci_runs_for_commit,
    /// The latest attempt of every independent workflow or grouped pipeline for a commit.
    /// Runs that belong to another pull request with the same head commit are left out.
    fn latest_ci_runs_for_commit(pool: &DbPool, repo_id: i64, sha: &str, pull_request_id: i64, target_branch: &str) -> Result<Vec<CiRun>>);

native_operation!(crate::backend::db::postgres::ci::list_pull_request_check_statuses,
    crate::backend::db::sqlite::ci::list_pull_request_check_statuses,
    fn list_pull_request_check_statuses(pool: &DbPool, pull_request_ids: &[i64]) -> Result<Vec<(i64, String)>>);

native_operation!(crate::backend::db::postgres::ci::list_ci_runs_for_user,
    crate::backend::db::sqlite::ci::list_ci_runs_for_user,
    fn list_ci_runs_for_user(pool: &DbPool, user_id: i64, limit: i64) -> Result<Vec<CiRun>>);

native_operation!(crate::backend::db::postgres::ci::append_ci_logs,
    crate::backend::db::sqlite::ci::append_ci_logs,
    fn append_ci_logs(
    pool: &DbPool,
    run_id: i64,
    worker: &str,
    lines: &[crate::backend::ci::queue::LogLine],
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::ci::list_ci_logs,
    crate::backend::db::sqlite::ci::list_ci_logs,
    fn list_ci_logs(pool: &DbPool, run_id: i64, limit: i64) -> Result<Vec<CiLogLine>>);

native_operation!(crate::backend::db::postgres::ci::list_ci_log_history,
    crate::backend::db::sqlite::ci::list_ci_log_history,
    fn list_ci_log_history(
    pool: &DbPool,
    run_id: i64,
    after: i64,
    limit: i64,
) -> Result<Vec<CiLogLine>>);

native_operation!(crate::backend::db::postgres::ci::get_ci_deployment_by_run_id,
    crate::backend::db::sqlite::ci::get_ci_deployment_by_run_id,
    fn get_ci_deployment_by_run_id(pool: &DbPool, run_id: i64) -> Result<Option<CiDeployment>>);

native_operation!(crate::backend::db::postgres::ci::ci_repository_owner,
    crate::backend::db::sqlite::ci::ci_repository_owner,
    fn ci_repository_owner(pool: &DbPool, repo_id: i64) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::ci::record_ci_accepted_push,
    crate::backend::db::sqlite::ci::record_ci_accepted_push,
    /// Remembers a commit that a push really delivered, so a later force-push
    /// doesn't stop CI from running against it.
    fn record_ci_accepted_push(
    pool: &DbPool,
    repo_id: i64,
    actor_id: i64,
    reference: &str,
    sha: &str,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::ci::has_ci_accepted_push,
    crate::backend::db::sqlite::ci::has_ci_accepted_push,
    fn has_ci_accepted_push(
    pool: &DbPool,
    repo_id: i64,
    actor_id: i64,
    reference: &str,
    sha: &str,
) -> Result<bool>);
