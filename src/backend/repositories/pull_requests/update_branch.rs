//! Bring a pull head up to date without merging or closing the pull request.
use super::{database_error, OperationError};
use crate::backend::{
    ci::{CiRuns, RefUpdate},
    db::{self, DbPool},
    repositories::store::{repo_store, ErrorKind, PullBranchUpdate, RepoError},
};

fn invalid(message: &str) -> OperationError {
    OperationError::Invalid {
        code: "validation_error",
        message: message.into(),
    }
}
fn storage(error: RepoError) -> OperationError {
    if error.is_write_denied() || error.is_protected_branch() || error.is_merge_commit_blocked() {
        return OperationError::Denied {
            code: "branch_protection",
            message: "Head branch write denied".into(),
        };
    }
    match error.kind {
        ErrorKind::Conflict | ErrorKind::Invalid | ErrorKind::NotFound | ErrorKind::MissingPath => {
            invalid("The base or head branch changed, is unavailable, or cannot be merged")
        }
        ErrorKind::Unavailable => OperationError::Unavailable {
            code: "storage_unavailable",
            message: "Repository storage is unavailable".into(),
        },
    }
}

pub async fn update_branch(
    pool: &DbPool,
    ci: &CiRuns,
    actor_id: i64,
    locator: (&str, &str, i32),
    expected: Option<&str>,
    token: Option<i64>,
) -> Result<String, OperationError> {
    let actor = super::actor(pool, actor_id).await?;
    let base = db::repositories::records::get_repository(pool, locator.0, locator.1)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    super::require_read(pool, &actor, &base).await?;
    if base.is_archived {
        return Err(OperationError::Denied {
            code: "write_denied",
            message: "Archived repositories cannot update pull requests".into(),
        });
    }
    let pull = db::pull_requests::records::get_pull_request(pool, base.id, locator.2)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".into(),
        })?;
    if pull.state != "open" {
        return Err(invalid(
            "Only open pull requests can update their head branch",
        ));
    }
    let head = super::head_repository(
        pool,
        &base,
        pull.head_repo_id
            .ok_or_else(|| invalid("Head branch is unavailable"))?,
    )
    .await
    .map_err(|error| match error {
        super::branches::HeadError::Unavailable => database_error(()),
        _ => invalid("Head branch is unavailable"),
    })?;
    if !super::maintainer_edit::can_update_head(pool, &actor, &base, &head, &pull)
        .await
        .map_err(database_error)?
    {
        return Err(OperationError::Denied {
            code: "write_denied",
            message: "Head branch write denied".into(),
        });
    }
    let source = repo_store(&base, ci.cluster.as_ref());
    let target = repo_store(&head, ci.cluster.as_ref());
    let head_sha = target
        .commit_from_primary(&format!("refs/heads/{}", pull.head_branch))
        .await
        .map_err(storage)?
        .sha;
    if expected.is_some_and(|value| !value.eq_ignore_ascii_case(&head_sha)) {
        return Err(invalid("expected_head_sha does not match the current head"));
    }
    let base_sha = source
        .commit_from_primary(&format!("refs/heads/{}", pull.base_branch))
        .await
        .map_err(storage)?
        .sha;
    let sha = target
        .update_pull_branch_as(
            pool,
            head.id,
            actor.id,
            (base.id, &source),
            &PullBranchUpdate {
                pull_request_id: pull.id,
                base_branch: pull.base_branch,
                branch: pull.head_branch.clone(),
                head: head_sha.clone(),
                base: base_sha,
                name: actor.username.clone(),
                email: actor.commit_email(),
                token,
            },
        )
        .await
        .map_err(storage)?;
    super::branches_moved(pool, &base, ci, actor.id).await;
    if head.id != base.id {
        super::branches_moved(pool, &head, ci, actor.id).await;
    }
    // Keep metadata repair above, but an unchanged ref is not a push event.
    if sha.eq_ignore_ascii_case(&head_sha) {
        return Ok(sha);
    }
    // The head moved, so its push workflows run as they do after any other push.
    let head_owner = db::repositories::records::repository_owner_name(pool, head.id)
        .await
        .map_err(database_error)?
        .ok_or_else(|| database_error(()))?;
    ci.queue_push_runs(
        &head,
        &head_owner,
        actor.id,
        RefUpdate {
            reference: format!("refs/heads/{}", pull.head_branch),
            old: Some(head_sha),
            new: sha.clone(),
            changed_paths: None,
        },
    )
    .await;
    Ok(sha)
}
