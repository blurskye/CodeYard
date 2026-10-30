//! Auto-merge: a writer asks for a pull request to merge once its requirements pass. A
//! background check then merges it through the ordinary merge path, as that writer, so
//! reviews, checks and branch protection apply exactly as they do to a merge button.
use super::{actor, merge, require_write, OperationError};
use crate::backend::{
    app::AppState,
    db::{self, models::PullRequest, pull_requests::auto_merge::AutoMerge},
    repositories::store::MergeMethod,
};
use std::{sync::Arc, time::Duration};

/// How often the background check retries pull requests waiting to merge.
const CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// Waiting pull requests read per page. A check walks every page, so a blocked request
/// cannot hide the ones behind it.
const CHECK_BATCH: i64 = 50;

fn storage_error<E>(_: E) -> OperationError {
    OperationError::Unavailable {
        code: "database_error",
        message: "Collaboration storage is temporarily unavailable".into(),
    }
}

pub struct EnableRequest {
    pub expected_head: Option<String>,
    pub token: Option<i64>,
    pub merge_method: MergeMethod,
    pub commit_title: Option<String>,
    pub commit_message: Option<String>,
    pub author_email: Option<String>,
}

async fn writable(
    pool: &db::DbPool,
    actor_id: i64,
    pull: &PullRequest,
) -> Result<(), OperationError> {
    let user = actor(pool, actor_id).await?;
    let repository = db::repositories::records::get_repository_by_id(pool, pull.repo_id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    require_write(pool, &user, &repository).await?;
    if pull.state != "open" {
        return Err(OperationError::Invalid {
            code: "not_open",
            message: "Pull request is not open".into(),
        });
    }
    Ok(())
}

/// Turns auto-merge on for `pull`, replacing an earlier request.
pub async fn enable(
    pool: &db::DbPool,
    actor_id: i64,
    pull: &PullRequest,
    request: EnableRequest,
) -> Result<(), OperationError> {
    writable(pool, actor_id, pull).await?;
    db::pull_requests::auto_merge::enable_auto_merge_at_head(
        pool,
        &AutoMerge {
            pull_request_id: pull.id,
            enabled_by: actor_id,
            merge_method: request.merge_method.as_str().into(),
            commit_title: request.commit_title,
            commit_message: request.commit_message,
            author_email: request.author_email,
            enabled_at: chrono::Utc::now(),
        },
        request.expected_head.as_deref(),
        request.token,
    )
    .await
    .map_err(super::write_error)
}

/// Turns auto-merge off for `pull`. Turning it off twice is not an error.
pub async fn disable(
    pool: &db::DbPool,
    actor_id: i64,
    pull: &PullRequest,
) -> Result<(), OperationError> {
    writable(pool, actor_id, pull).await?;
    db::pull_requests::auto_merge::disable_auto_merge(pool, pull.id)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// Tries every waiting pull request once. A merge that its requirements still refuse waits
/// for the next check; a pull request that closed or merged some other way drops its request.
pub async fn run_due(state: &AppState) -> anyhow::Result<()> {
    run_due_until_stop(state, None).await
}

async fn run_due_until_stop(state: &AppState, stop: Option<&tokio::sync::watch::Receiver<bool>>) -> anyhow::Result<()> {
    let pool = &state.pool;
    let mut after = 0;
    loop {
        let page =
            db::pull_requests::auto_merge::pending_auto_merges(pool, after, CHECK_BATCH).await?;
        let (Some(last), short) = (
            page.last().map(|entry| entry.pull_request_id),
            (page.len() as i64) < CHECK_BATCH,
        ) else {
            return Ok(());
        };
        for entry in page {
            if stop.is_some_and(|stop| *stop.borrow()) {
                return Ok(());
            }
            let pull =
                db::pull_requests::records::get_pull_request_by_id(pool, entry.pull_request_id)
                    .await?;
            let repository = match &pull {
                Some(pull) if pull.state == "open" => {
                    db::repositories::records::get_repository_by_id(pool, pull.repo_id).await?
                }
                _ => None,
            };
            let owner = match &repository {
                Some(repository) => {
                    db::repositories::records::repository_owner_name(pool, repository.id).await?
                }
                None => None,
            };
            let (Some(pull), Some(repository), Some(owner)) = (pull, repository, owner) else {
                db::pull_requests::auto_merge::disable_auto_merge(pool, entry.pull_request_id)
                    .await?;
                continue;
            };
            // The page was read before earlier merges ran, so this request may have been
            // disabled or replaced since. Only the request as it stands now may merge.
            let current =
                db::pull_requests::auto_merge::get_auto_merge(pool, entry.pull_request_id).await?;
            if current.is_none_or(|current| current.enabled_at != entry.enabled_at) {
                continue;
            }
            let merged = merge::merge_pull_request(
                pool,
                &state.ci_runs,
                &state.notification_broadcaster,
                entry.enabled_by,
                (owner, repository.name, pull.number),
                merge::MergeRequest {
                    expected_head: pull.head_sha,
                    expected_base: pull.base_sha,
                    expected_base_branch: Some(pull.base_branch),
                    expected_target: None,
                    merge_method: MergeMethod::parse(&entry.merge_method).unwrap_or_default(),
                    commit_title: entry.commit_title,
                    commit_message: entry.commit_message,
                    author_email: entry.author_email,
                    token: None,
                },
            )
            .await;
            match merged {
                Ok(_) | Err(OperationError::BranchMerged { .. }) => {
                    db::pull_requests::auto_merge::disable_auto_merge(pool, pull.id).await?;
                }
                Err(error) => {
                    tracing::debug!(pull_request = pull.id, %error, "auto-merge is still waiting");
                }
            }
        }
        if short {
            return Ok(());
        }
        after = last;
    }
}

/// A check is never cut short: a merge in progress finishes before the loop stops.
pub async fn run(state: Arc<AppState>, mut stop: tokio::sync::watch::Receiver<bool>) {
    while !*stop.borrow() {
        if let Err(error) = run_due_until_stop(&state, Some(&stop)).await {
            tracing::warn!(%error, "auto-merge check failed");
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(CHECK_INTERVAL) => {}
        }
    }
}
