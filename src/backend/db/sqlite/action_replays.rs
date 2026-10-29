use crate::backend::db::{
    action_jobs::Job,
    action_replays,
    ci::{CiAdmissionRefusal as Refusal, CiReplaySelection, CiRunOptions, NewCiAttempt},
    models::CiRun,
};
use anyhow::{ensure, Result};
use codeyard_actions::progress::{Replay, State};
use sqlx::{types::Json, SqliteConnection};

pub(crate) async fn prepare(
    connection: &mut SqliteConnection,
    source: &CiRun,
    request: &NewCiAttempt,
) -> Result<Option<Replay>> {
    if request.selection == CiReplaySelection::All {
        return Ok(None);
    }
    let root = source.workflow_run_id.unwrap_or(source.id);
    let current:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM ci_runs WHERE repo_id=$1 AND workflow_run_id=$2 AND workflow_run_attempt>$3)").bind(source.repo_id).bind(root).bind(source.workflow_run_attempt.unwrap_or(1)).fetch_one(&mut *connection).await?;
    ensure!(current, Refusal::Conflict("Rerun source changed"));
    let native:bool=sqlx::query_scalar("SELECT CASE WHEN r.actions_engine='codeyard-actions' AND COALESCE(s.codeyard_actions_enabled,TRUE) THEN TRUE WHEN r.actions_engine='act' AND COALESCE(s.act_enabled,TRUE) THEN FALSE ELSE COALESCE(s.default_engine,'codeyard-actions')='codeyard-actions' END FROM repositories r LEFT JOIN actions_settings s ON s.id=1 WHERE r.id=$1").bind(source.repo_id).fetch_one(&mut *connection).await?;
    ensure!(
        native,
        Refusal::Conflict("Partial re-runs require the codeyard-actions engine")
    );
    let options: Option<CiRunOptions> =
        sqlx::query_as("SELECT data AS replay,debug FROM actions_replays WHERE run_id=$1")
            .bind(source.id)
            .fetch_optional(&mut *connection)
            .await?;
    let fresh:Option<Json<State>>=sqlx::query_scalar("SELECT s.state FROM actions_job_states s JOIN actions_runtime_tokens t ON t.run_id=s.run_id AND t.token_hash=s.generation WHERE s.run_id=$1 AND s.workflow_path=$2").bind(source.id).bind(&source.workflow_path).fetch_optional(&mut *connection).await?;
    let fresh = fresh
        .ok_or(Refusal::Conflict(
            "Native dependency state is unavailable; re-run the whole workflow",
        ))?
        .0;
    let state = action_replays::compose(
        options
            .and_then(|options| options.replay)
            .map(|data| data.0),
        fresh,
    );
    let observations:Vec<Job>=sqlx::query_as("SELECT j.* FROM actions_jobs j JOIN ci_runs c ON c.id=j.run_id JOIN actions_runtime_tokens t ON t.run_id=j.run_id AND t.token_hash=j.generation WHERE c.repo_id=$1 AND COALESCE(c.workflow_run_id,c.id)=$2 AND COALESCE(c.workflow_run_attempt,1)<=$3 AND j.workflow_path=$4 ORDER BY COALESCE(c.workflow_run_attempt,1),j.id").bind(source.repo_id).bind(root).bind(source.workflow_run_attempt.unwrap_or(1)).bind(&source.workflow_path).fetch_all(&mut *connection).await?;
    action_replays::build(source, request, state, observations)
}
