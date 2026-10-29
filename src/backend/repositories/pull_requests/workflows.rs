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
/// Queues the workflows that `pulls` now need on their new base.
/// Let admission finish: cancelling it after a saved revision would lose these runs.
pub async fn queue_moved_pull_requests_ci(
    pool: &DbPool,
    ci: &CiRuns,
    repository: &Repository,
    pulls: &[PullRequest],
    actor_id: i64,
) {
    for pull in pulls {
        let _ = queue_pull_request_ci(
            pool,
            ci,
            repository,
            pull,
            actor_id,
            false,
            crate::backend::db::models::AccessLevel::Owner,
        )
        .await;
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
    access: db::models::AccessLevel,
) -> Result<(), AdmissionError> {
    if pull.state != "open" || pull.is_draft {
        return Ok(());
    }
    let owner = db::ci::ci_repository_owner(pool, repository.id)
        .await?
        .ok_or(AdmissionError::Unavailable)?;
    let author = db::users::accounts::get_user_by_id(pool, pull.author_id)
        .await?
        .ok_or(AdmissionError::Unavailable)?;
    let sender = db::users::accounts::get_user_by_id(pool, actor_id)
        .await?
        .ok_or(AdmissionError::Unavailable)?;
    use crate::backend::webhooks::payloads::{self, Person, Scope};
    let mut payload = payloads::pull_request(
        "synchronize",
        pull,
        &Person::of(&author),
        Scope {
            repo: repository,
            owner: &owner,
            sender: &Person::of(&sender),
        },
    );
    crate::backend::webhooks::set_open_issues(pool, repository.id, &mut payload).await;
    queue_pull_request_workflow_event(
        pool,
        ci,
        repository,
        pull,
        actor_id,
        manual,
        access,
        "pull_request",
        &payload,
    )
    .await
}

/// The exact webhook document is also the workflow's event document.
pub(crate) async fn queue_pull_request_event_ci(
    pool: &DbPool,
    ci: &CiRuns,
    repository: &Repository,
    pull: &PullRequest,
    actor_id: i64,
    access: db::models::AccessLevel,
    payload: &serde_json::Value,
) {
    if payload.is_null() {
        return;
    }
    for event in ["pull_request", "pull_request_target"] {
        if let Err(error) = queue_pull_request_workflow_event(
            pool, ci, repository, pull, actor_id, false, access, event, payload,
        )
        .await
        {
            tracing::warn!(repo_id=repository.id, pull_number=pull.number, %error, event, "pull request event workflows were not queued");
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "A pull request event carries its immutable webhook document and execution authority."
)]
async fn queue_pull_request_workflow_event(
    pool: &DbPool,
    ci: &CiRuns,
    repository: &Repository,
    pull: &PullRequest,
    actor_id: i64,
    manual: bool,
    access: db::models::AccessLevel,
    event: &str,
    payload: &serde_json::Value,
) -> Result<(), AdmissionError> {
    let target = event == "pull_request_target";
    if !target && (pull.is_draft || (pull.state != "open" && payload["action"] != "closed")) {
        return Ok(());
    }
    let awaiting_approval = !target
        && !manual
        && pull.head_repo_id != Some(repository.id)
        && pull.workflows_approved_sha.as_deref() != Some(&pull.head_sha);
    let may_discover = if awaiting_approval {
        db::repositories::access::get_user_repo_access(pool, actor_id, repository.id)
            .await
            .map(|access| access.can_read())
    } else {
        ci.actor_may_discover_workflows(repository.id, actor_id)
            .await
    };
    match may_discover {
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
    let (trunk, base_sha) = if target {
        (pull.base_branch.clone(), pull.base_sha.clone())
    } else {
        let (branch, sha, _) = super::stacks::merge_target(pool, pull)
            .await
            .map_err(|_| AdmissionError::Unavailable)?;
        (branch, sha)
    };
    let store = repo_store(repository, ci.cluster.as_ref());
    let revision = if target { &base_sha } else { &pull.head_sha };
    let workflows = match list_workflows_for_ref(&store, revision).await {
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
        if !workflow.matches_activity(event, payload["action"].as_str().unwrap_or_default()) {
            continue;
        }
        match workflow.matches_event_with_budget(event, &trunk, &mut match_budget) {
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

    let message = if target {
        None
    } else {
        Some(commit_message_for_ref(&store, &pull.head_sha).await.map_err(|_| AdmissionError::Unavailable)?)
    };
    if message.as_deref().is_some_and(should_skip_run_from_message) {
        return Ok(());
    }
    let needs_paths = applicable.iter().any(|workflow| {
        workflow
            .pull_request_filter(event)
            .is_some_and(|filter| !filter.paths.is_empty() || !filter.paths_ignore.is_empty())
    });
    let mut first_failure = None;
    let paths = if needs_paths {
        match store.changed_paths(&base_sha, &pull.head_sha).await {
            Ok(paths) => Some(paths),
            Err(error) => {
                tracing::warn!(
                    repo_id = repository.id,
                    pull_number = pull.number,
                    %error,
                    "pull request changed paths unavailable; path-filtered workflows skipped"
                );
                first_failure = Some(AdmissionError::Unavailable);
                None
            }
        }
    } else {
        Some(Vec::new())
    };

    let mut matched = Vec::new();
    for workflow in applicable {
        if paths.is_none()
            && workflow
                .pull_request_filter(event)
                .is_some_and(|filter| !filter.paths.is_empty() || !filter.paths_ignore.is_empty())
        {
            continue;
        }
        match workflow.matches_event_with_context_and_budget(
            event,
            &trunk,
            paths.as_deref().unwrap_or_default(),
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
        return first_failure.map_or(Ok(()), Err);
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
    for workflow in matched {
        let pipeline = infer_pipeline_from_workflow_path(&workflow.path);
        if let Err(error) = ci
            .trigger_event_run(
                CiRunRequest {
                    repo_id: repository.id,
                    repo_owner: owner.clone(),
                    repo_name: repository.name.clone(),
                    branch: trunk.clone(),
                    head_sha: Some(revision.clone()),
                    event: event.into(),
                    pull_request_id: Some(pull.id),
                    pipeline,
                    workflow_paths: vec![workflow.path],
                    environment: None,
                    inputs: None,
                    triggered_by_id: actor_id,
                    manual,
                },
                access,
                payload.clone(),
                None,
            )
            .await
        {
            if !manual && matches!(error, AdmissionError::WorkflowDisabled) {
                continue;
            }
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
    repository: &db::models::Repository,
    number: i32,
    request: RunWorkflowsRequest,
) -> Result<db::models::PullRequest, OperationError> {
    // The caller resolved and authorized this repository; a rename must not switch it.
    let user = super::actor(pool, actor_id).await?;
    super::require_write(pool, &user, repository).await?;
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
    super::branches::refresh_open_pull_request(pool, repository, ci, user.id, pull.id, None)
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
    if pull.head_sha != request.expected_head {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message: "Pull request head changed; reload before running workflows".into(),
        });
    }
    queue_pull_request_ci(
        pool,
        ci,
        repository,
        &pull,
        user.id,
        true,
        crate::backend::db::models::AccessLevel::Owner,
    )
    .await
    .map_err(workflows_not_queued)?;
    Ok(pull)
}
