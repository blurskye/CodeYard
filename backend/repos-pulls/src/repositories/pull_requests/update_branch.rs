//! Bring a pull head up to date without merging or closing the pull request.
use codeyard_ci_engine::runs::CiRuns;
use codeyard_database::DbPool;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git_storage::push_workflows::RefUpdate;
use codeyard_git_storage::repositories::pull_requests::database_error;
use codeyard_git_storage::repositories::pull_requests::OperationError;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_git_storage::store_types::PullBranchUpdate;

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
    rebase: bool,
    token: Option<i64>,
) -> Result<String, OperationError> {
    let actor = codeyard_git_storage::repositories::pull_requests::actor(pool, actor_id).await?;
    let base = codeyard_db_repos::repositories::records::get_repository(pool, locator.0, locator.1)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    codeyard_git_storage::repositories::pull_requests::require_read(pool, &actor, &base).await?;
    if base.is_archived {
        return Err(OperationError::Denied {
            code: "write_denied",
            message: "Archived repositories cannot update pull requests".into(),
        });
    }
    let pull =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, base.id, locator.2)
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
    let head = codeyard_ci_engine::pull_requests::branches::head_repository(
        pool,
        &base,
        pull.head_repo_id
            .ok_or_else(|| invalid("Head branch is unavailable"))?,
    )
    .await
    .map_err(|error| match error {
        codeyard_ci_engine::pull_requests::branches::HeadError::Unavailable => database_error(()),
        _ => invalid("Head branch is unavailable"),
    })?;
    if !codeyard_repos::maintainer_edit::can_update_head(pool, &actor, &base, &head, &pull)
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
    let head_ref = format!("refs/heads/{}", pull.head_branch);
    let updated = target
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
                rebase,
                name: actor.username.clone(),
                email: actor.commit_email(),
                token,
            },
        )
        .await;
    // When only the repository metadata failed, the branch already moved: the pull request
    // and its workflows still follow the published tip, and the failure is reported after.
    let (sha, followup_failed) = match updated {
        Ok(sha) => (Some(sha), false),
        Err(error) if error.is_applied_followup_failed() => (
            target
                .commit_from_primary(&head_ref)
                .await
                .ok()
                .map(|commit| commit.sha),
            true,
        ),
        Err(error) => return Err(storage(error)),
    };
    // A head writer's own update is published with their authority; the wider refresh follows.
    if let Err(error) = codeyard_ci_engine::pull_requests::branches::refresh_updated_head(
        pool, &base, &head, ci, actor.id, pull.id,
    )
    .await
    {
        tracing::warn!(pull_request_id = pull.id, %error, "could not refresh the updated pull request");
    }
    codeyard_ci_engine::pull_requests::branches::branches_moved(pool, &base, ci, actor.id).await;
    if head.id != base.id {
        codeyard_ci_engine::pull_requests::branches::branches_moved(pool, &head, ci, actor.id)
            .await;
    }
    let finish = |sha: String| {
        if followup_failed {
            Err(OperationError::Unavailable {
                code: "update_followup_failed",
                message: "The head branch was updated, but repository metadata could not be saved."
                    .into(),
            })
        } else {
            Ok(sha)
        }
    };
    let Some(sha) = sha else {
        return finish(String::new());
    };
    // Keep metadata repair above, but an unchanged ref is not a push event.
    if sha.eq_ignore_ascii_case(&head_sha) {
        return finish(sha);
    }
    // The head moved, so its push workflows run as they do after any other push.
    let head_owner = codeyard_db_repos::repositories::records::repository_owner_name(pool, head.id)
        .await
        .map_err(database_error)?
        .ok_or_else(|| database_error(()))?;
    ci.queue_push_runs(
        &head,
        &head_owner,
        actor.id,
        RefUpdate {
            reference: head_ref,
            old: Some(head_sha),
            new: sha.clone(),
            changed_paths: None,
        },
    )
    .await;
    finish(sha)
}
