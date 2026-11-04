//! Durable asynchronous requests, executed by the server's bounded, shutdown-drained Git tasks.
use crate::services::RepositoryServices;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_ci_engine::runs::CiRuns;
use codeyard_repos_core::repositories::pull_requests::database_error;
use codeyard_repos_core::repositories::pull_requests::OperationError;
use serde_json::json;
use serde_json::Value;

pub struct Job {
    pub repo_id: i64,
    pub pull_request_id: i64,
    /// The token the request came with. The job runs on this server, so the credential stays
    /// in memory and is rechecked before preparation and publication.
    pub authority: codeyard_db_core::queries::credentials::Actor,
    pub owner: String,
    pub name: String,
    pub number: i32,
    pub merge: crate::repositories::pull_requests::merge::MergeRequest,
    pub action: String,
}
fn denied() -> OperationError {
    OperationError::Denied {
        code: "token_revoked",
        message: "The merge token expired, was revoked, or no longer permits writes".into(),
    }
}
async fn execute(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    broadcaster: &std::sync::Arc<NotificationBroadcaster>,
    uuid: &str,
    job: Job,
) -> Result<crate::repositories::pull_requests::merge::MergeResponse, OperationError> {
    let token_id = job.authority.token_id().ok_or_else(denied)?;
    let actor = codeyard_db_identity::users::tokens::token_user(pool, token_id, true)
        .await
        .map_err(database_error)?
        .filter(|u| u.id == job.authority.user_id)
        .ok_or_else(denied)?;
    let repo = codeyard_db_repos::repositories::records::get_repository_by_id(pool, job.repo_id)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    let owner = codeyard_db_repos::repositories::records::repository_owner_names(pool, &[repo.id])
        .await
        .map_err(database_error)?
        .remove(&repo.id)
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository owner not found".into(),
        })?;
    let mut prepared = crate::repositories::pull_requests::merge::prepare_pull_request_merge(
        pool,
        ci,
        job.authority.clone(),
        (owner, repo.name, job.number),
        job.merge,
    )
    .await?;
    if !prepared.matches_request(job.repo_id, job.pull_request_id) {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message: "The pull request's repository changed".into(),
        });
    }
    prepared.bind_async_request(uuid, token_id);
    // Preparation can wait on Git. Recheck the original credential immediately before publication.
    codeyard_db_identity::users::tokens::token_user(pool, token_id, true)
        .await
        .map_err(database_error)?
        .filter(|u| u.id == actor.id)
        .ok_or_else(denied)?;
    let row = codeyard_db_repos::pull_requests::async_merge::get(
        pool,
        job.repo_id,
        job.pull_request_id,
        uuid,
    )
    .await
    .map_err(database_error)?
    .filter(|row| row.status == "pending")
    .ok_or_else(|| OperationError::Conflict {
        code: "merge_cancelled",
        message: "The merge request is no longer pending".into(),
    })?;
    if row.token_id != Some(token_id) {
        return Err(denied());
    }
    crate::repositories::pull_requests::merge::publish_prepared_merge(
        pool,
        ci,
        broadcaster,
        prepared,
    )
    .await
}
pub async fn start(state: &dyn RepositoryServices, job: Job) -> Result<Value, OperationError> {
    if !matches!(job.action.as_str(), "default" | "direct_merge") {
        return Err(OperationError::Invalid {
            code: "merge_action",
            message: "Merge queues are not available".into(),
        });
    }
    let token_id = job.authority.token_id().ok_or_else(denied)?;
    let actor = codeyard_db_identity::users::tokens::token_user(state.pool(), token_id, true)
        .await
        .map_err(database_error)?
        .filter(|u| u.id == job.authority.user_id)
        .ok_or_else(denied)?;
    let repo =
        codeyard_db_repos::repositories::records::get_repository_by_id(state.pool(), job.repo_id)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Repository not found".into(),
            })?;
    codeyard_repos_core::repositories::pull_requests::require_write(state.pool(), &actor, &repo)
        .await?;
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
        state.pool(),
        job.pull_request_id,
    )
    .await
    .map_err(database_error)?
    .filter(|p| p.repo_id == job.repo_id && p.number == job.number)
    .ok_or_else(|| OperationError::Missing {
        code: "not_found",
        message: "Pull request not found".into(),
    })?;
    if pull.state != "open" || pull.merged || pull.is_draft {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Only an open, non-draft pull request can be merged".into(),
        });
    }
    if pull.head_sha != job.merge.expected_head || pull.base_sha != job.merge.expected_base {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message: "The pull request branches changed".into(),
        });
    }
    let uuid = uuid::Uuid::new_v4().to_string();
    let details = json!({"message":"Merge request accepted","uuid":uuid,"merge_method":job.merge.merge_method.as_str(),"merge_action":job.action,"expected_head_sha":job.merge.expected_head,"bypass_rules":false});
    let request = serde_json::to_value(&job.merge).map_err(database_error)?;
    if !codeyard_db_repos::pull_requests::async_merge::insert(
        state.pool(),
        codeyard_db_repos::pull_requests::async_merge::NewAsyncMerge {
            uuid: &uuid,
            repo_id: job.repo_id,
            pull_request_id: job.pull_request_id,
            token_id,
            worker_node_id: state.cluster().map(|n| n.node_id),
            owner: &job.owner,
            name: &job.name,
            number: job.number,
            request: &request,
            details: &details,
        },
    )
    .await
    .map_err(database_error)?
    {
        return Err(OperationError::Conflict {
            code: "merge_pending",
            message: "A merge request is already pending for this pull request".into(),
        });
    }
    let (pool, ci, broadcaster) = (
        state.pool().clone(),
        state.ci_runs().clone(),
        state.notification_broadcaster().clone(),
    );
    let task_uuid = uuid.clone();
    if !state.git_tasks().spawn(async move {
        let (status,details)=match execute(&pool,&ci,&broadcaster,&task_uuid,job).await {
            Ok(result) if result.merged=>("merged",json!({"message":result.message,"sha":result.sha})),
            Ok(result)=>("failed",json!({"message":result.message})),
            Err(OperationError::BranchMerged{sha})=>("merged",json!({"message":"The branch merged, but pull request bookkeeping failed","sha":sha})),
            Err(error)=>("failed",json!({"message":error.to_string()})),
        };
        finish_with_retry(&pool, &task_uuid, status, &details).await;
    }) {
        codeyard_db_repos::pull_requests::async_merge::finish(state.pool(),&uuid,"failed",&json!({"message":"The merge worker is unavailable"})).await.map_err(database_error)?;
        return Err(OperationError::Unavailable{code:"merge_worker_unavailable",message:"The merge worker is unavailable".into()});
    }
    Ok(json!({"status":"pending","details":details}))
}
/// Records the result, retrying a failed write with backoff. The update only changes a pending
/// row, so repeating it is safe. When every attempt fails, restart recovery settles the row.
async fn finish_with_retry(
    pool: &codeyard_db_core::DbPool,
    uuid: &str,
    status: &str,
    details: &Value,
) {
    const ATTEMPTS: u32 = 8;
    let mut delay = std::time::Duration::from_millis(250);
    for attempt in 1..=ATTEMPTS {
        match codeyard_db_repos::pull_requests::async_merge::finish(pool, uuid, status, details)
            .await
        {
            Ok(()) => return,
            Err(error) if attempt == ATTEMPTS => {
                tracing::error!(%error, uuid, "could not persist asynchronous merge result")
            }
            Err(error) => {
                tracing::warn!(%error, uuid, attempt, "asynchronous merge result not saved; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }
}
/// Only this node's abandoned requests fail; starting another web node never cancels live work.
/// A request whose pull request is already recorded as merged reports that merge instead.
pub async fn recover(pool: &codeyard_db_core::DbPool, node_id: Option<i64>) -> anyhow::Result<u64> {
    codeyard_db_repos::pull_requests::async_merge::recover(
        pool,
        node_id,
        &json!({"message":"The server restarted before this merge request completed"}),
    )
    .await
}
