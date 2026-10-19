use super::OperationError;
use crate::backend::ci::admission::AdmissionError;
use crate::backend::ci::commit_message_for_ref;
use crate::backend::ci::infer_pipeline_from_workflow_path;
use crate::backend::ci::list_workflows_for_ref;
use crate::backend::ci::should_skip_run_from_message;
use crate::backend::ci::workflows::WorkflowMatchBudget;
use crate::backend::ci::CiRunRequest;
use crate::backend::ci::CiRuns;
use crate::backend::db;
use crate::backend::db::models::PullRequest;
use crate::backend::db::models::Repository;
use crate::backend::db::DbPool;
use crate::backend::repositories::store::repo_store;
use anyhow::Context;
use std::time::Duration;

/// How long a saved change waits to queue the workflows of the pull requests it moved. The change
/// is already final, so a slow admission check or storage node must not hold up its response.
pub(super) const MOVED_CI_WAIT: Duration = Duration::from_secs(5);

/// Queues the workflows that `pulls` now need on their new base, for at most `MOVED_CI_WAIT`.
/// Queueing still running when the wait ends is abandoned and logged.
pub async fn queue_moved_pull_requests_ci(
    pool: &DbPool,
    ci: &CiRuns,
    repository: &Repository,
    pulls: &[PullRequest],
    actor_id: i64,
) {
    let queued = tokio::time::timeout(MOVED_CI_WAIT, async {
        for pull in pulls {
            let _ = queue_pull_request_ci(pool, ci, repository, pull, actor_id, false).await;
        }
    })
    .await;
    if queued.is_err() {
        tracing::warn!(
            repo_id = repository.id,
            count = pulls.len(),
            "queueing workflows for moved pull requests did not finish in time"
        );
    }
}

/// Runs head code and workflows, filtering on the base branch. Automatic admission queues
/// each workflow once per commit, and fork heads need explicit approval. `manual` is set
/// when a person asked for the workflows, which starts them again after a finished run.
/// Returns the first reason a workflow was not queued. A refused workflow does not stop
/// the others. Automatic callers ignore the result; an explicit approval reports it.
pub async fn queue_pull_request_ci(
    pool: &DbPool,
    ci: &CiRuns,
    repository: &Repository,
    pull: &PullRequest,
    actor_id: i64,
    manual: bool,
) -> Result<(), AdmissionError> {
    if pull.state != "open"
        || pull.is_draft
        || (pull.head_repo_id != Some(repository.id)
            && pull.workflows_approved_sha.as_deref() != Some(&pull.head_sha))
    {
        return Ok(());
    }
    match ci
        .actor_may_discover_workflows(repository.id, actor_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(AdmissionError::Denied(
                "CI host execution is not approved for this actor",
            ))
        }
        Err(error) => {
            tracing::warn!(
                repo_id = repository.id,
                pull_number = pull.number,
                %error,
                "CI authority unavailable before pull request workflow discovery"
            );
            return Err(AdmissionError::Unavailable);
        }
    }
    // A layer runs its workflows for the branch its stack merges into, not for the layer below it.
    let (trunk, _) = super::stacks::merge_target(pool, pull)
        .await
        .map_err(|_| AdmissionError::Unavailable)?;
    let store = repo_store(repository, ci.cluster.as_ref());
    let workflows = match list_workflows_for_ref(&store, &pull.head_sha).await {
        Ok(workflows) => workflows,
        Err(_) => {
            tracing::warn!(
                repo_id = repository.id,
                pull_number = pull.number,
                "CI workflow discovery unavailable for pull request"
            );
            return Err(AdmissionError::Unavailable);
        }
    };
    let mut match_budget = WorkflowMatchBudget::new();
    let mut applicable = Vec::new();
    for workflow in workflows {
        match workflow.matches_event_with_budget("pull_request", &trunk, &mut match_budget) {
            Ok(true) => applicable.push(workflow),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    repo_id = repository.id,
                    pull_number = pull.number,
                    %error,
                    "pull request workflow matching stopped at its work limit"
                );
                return Err(AdmissionError::Invalid(
                    "pull request workflow matching stopped at its work limit",
                ));
            }
        }
    }
    if applicable.is_empty() {
        return Ok(());
    }

    let message = commit_message_for_ref(&store, &pull.head_sha).await;
    if message.as_deref().is_some_and(should_skip_run_from_message) {
        return Ok(());
    }
    let needs_paths = applicable.iter().any(|workflow| {
        workflow
            .triggers
            .pull_request
            .as_ref()
            .is_some_and(|filter| !filter.paths.is_empty() || !filter.paths_ignore.is_empty())
    });
    let paths = if needs_paths {
        match store.changed_paths(&pull.base_sha, &pull.head_sha).await {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(
                    repo_id = repository.id,
                    pull_number = pull.number,
                    %error,
                    "pull request changed paths unavailable; workflows skipped"
                );
                return Err(AdmissionError::Unavailable);
            }
        }
    } else {
        Vec::new()
    };

    let mut matched = Vec::new();
    for workflow in applicable {
        match workflow.matches_event_with_context_and_budget(
            "pull_request",
            &trunk,
            &paths,
            message.as_deref(),
            &mut match_budget,
        ) {
            Ok(true) => matched.push(workflow),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    repo_id = repository.id,
                    pull_number = pull.number,
                    %error,
                    "pull request workflow matching stopped at its work limit"
                );
                return Err(AdmissionError::Invalid(
                    "pull request workflow matching stopped at its work limit",
                ));
            }
        }
    }
    if matched.is_empty() {
        return Ok(());
    }
    let owner = match db::ci::ci_repository_owner(pool, repository.id)
        .await
        .context("CI owner unavailable")
    {
        Ok(Some(owner)) => owner,
        _ => {
            tracing::warn!(
                repo_id = repository.id,
                pull_number = pull.number,
                "CI owner unavailable for pull request"
            );
            return Err(AdmissionError::Unavailable);
        }
    };
    let mut first_failure = None;
    for workflow in matched {
        let pipeline = infer_pipeline_from_workflow_path(&workflow.path);
        if let Err(error) = ci
            .trigger_run(CiRunRequest {
                repo_id: repository.id,
                repo_owner: owner.clone(),
                repo_name: repository.name.clone(),
                branch: trunk.clone(),
                head_sha: Some(pull.head_sha.clone()),
                event: "pull_request".into(),
                pull_request_id: Some(pull.id),
                pipeline,
                workflow_paths: vec![workflow.path],
                environment: None,
                inputs: None,
                triggered_by_id: actor_id,
                manual,
            })
            .await
        {
            tracing::warn!(
                repo_id = repository.id,
                pull_number = pull.number,
                %error,
                "workflow was not admitted for pull request"
            );
            first_failure = first_failure.or(Some(error));
        }
    }
    first_failure.map_or(Ok(()), Err)
}

/// The approval stays recorded, so the same head can be sent again once the cause is
/// fixed. Workflows that were already admitted are not queued twice.
fn workflows_not_queued(error: AdmissionError) -> OperationError {
    let message = format!(
        "Workflows were approved for this head, but a workflow was not queued: {error}. Run workflows again once the cause is fixed."
    );
    match error {
        AdmissionError::Denied(_) => OperationError::Denied {
            code: "ci_not_queued",
            message,
        },
        AdmissionError::Unavailable => OperationError::Unavailable {
            code: "ci_not_queued",
            message,
        },
        _ => OperationError::Conflict {
            code: "ci_not_queued",
            message,
        },
    }
}

#[derive(serde::Deserialize)]
pub struct RunWorkflowsRequest {
    pub expected_head: String,
}

pub async fn run_pull_request_workflows(
    pool: &db::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    request: RunWorkflowsRequest,
) -> Result<db::models::PullRequest, OperationError> {
    let repository = db::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(super::write_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    let user = super::actor(pool, actor_id).await?;
    super::require_write(pool, &user, &repository).await?;
    // Only trusted actors can queue these workflows, so no one else may record an
    // approval for them.
    if !user.is_admin && !ci.config.trusted_actor_ids.contains(&user.id) {
        return Err(OperationError::Denied {
            code: "ci_not_approved",
            message: "CI host execution is not approved for your account".to_owned(),
        });
    }
    let pull = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(super::write_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    super::branches::refresh_open_pull_request(pool, &repository, ci, user.id, pull.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "git_unavailable",
            message: "Pull request revisions could not be refreshed; workflows were not approved."
                .into(),
        })?;
    let pull = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
        .await
        .map_err(super::write_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    if !db::pull_requests::records::approve_pull_request_workflows(
        pool,
        pull.id,
        user.id,
        &request.expected_head,
    )
    .await
    .map_err(super::write_error)?
    {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message: "Pull request or permission changed; reload before running workflows"
                .to_owned(),
        });
    }
    let pull = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
        .await
        .map_err(|_| OperationError::Updated { number })?
        .ok_or(OperationError::Updated { number })?;
    queue_pull_request_ci(pool, ci, &repository, &pull, user.id, true)
        .await
        .map_err(workflows_not_queued)?;
    Ok(pull)
}
