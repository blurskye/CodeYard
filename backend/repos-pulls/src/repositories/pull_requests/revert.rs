//! Revert through the normal branch publication and pull request creation paths.
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_ci_engine::runs::CiRuns;
use codeyard_repos_core::repositories::pull_requests::database_error;
use codeyard_repos_core::repositories::pull_requests::OperationError;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_repos_core::store_types::RevertRequest as RevertCommit;

pub struct RevertRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    pub is_draft: bool,
}

pub async fn revert_pull_request(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    (owner, name, number): (String, String, i32),
    request: RevertRequest,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    let user = codeyard_repos_core::repositories::pull_requests::actor(pool, actor).await?;
    let repo = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &name)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    codeyard_repos_core::repositories::pull_requests::require_write(pool, &user, &repo).await?;
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request(pool, repo.id, number)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".into(),
        })?;
    let merged = pull
        .merge_commit_sha
        .as_deref()
        .filter(|_| pull.merged)
        .ok_or_else(|| OperationError::Invalid {
            code: "invalid_state",
            message: "Only merged pull requests can be reverted".into(),
        })?;
    // A stack merges in one commit. Its inverse for a lower layer would also remove the layers
    // above, so only the top layer can be reverted.
    if codeyard_db_repos::pull_requests::records::merged_above(
        pool,
        repo.id,
        merged,
        &pull.head_branch,
    )
    .await
    .map_err(database_error)?
    {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "This pull request was merged with the stack above it; revert the top pull request instead".into(),
        });
    }
    // The merge commit landed on the base of the bottom layer it merged, at the tip recorded for
    // that layer. A stacked layer's own base names the layer below it, which the merge left alone.
    let (base_branch, before) =
        codeyard_db_repos::pull_requests::records::merged_trunk(pool, repo.id, merged)
            .await
            .map_err(database_error)?
            .unwrap_or_else(|| (pull.base_branch.clone(), pull.base_sha.clone()));
    // The default title shortens a long original so it fits the 255-byte title limit.
    let title = request.title.unwrap_or_else(|| {
        let mut end = pull.title.len().min(255 - "Revert \"\"".len());
        while !pull.title.is_char_boundary(end) {
            end -= 1;
        }
        format!("Revert \"{}\"", &pull.title[..end])
    });
    let body = request
        .body
        .unwrap_or_else(|| format!("Reverts {owner}/{name}#{number}"));
    codeyard_model::issue_input::validate_title(&title)
        .and_then(|_| codeyard_model::issue_input::validate_body(&body, false))
        .map_err(|message| OperationError::Invalid {
            code: "validation_error",
            message: message.into(),
        })?;
    let store = repo_store(&repo, ci.cluster.as_ref());
    let current = store
        .commit_from_primary(&format!("refs/heads/{base_branch}"))
        .await
        .map_err(crate::repositories::pull_requests::merge::prepare_error)?
        .sha;
    let sha = store
        .prepare_revert(&RevertCommit {
            current,
            merged: merged.into(),
            before,
            message: format!("Revert \"{}\"\n\nThis reverts commit {merged}.", pull.title),
            name: user.username.clone(),
            email: user.commit_email(),
        })
        .await
        .map_err(crate::repositories::pull_requests::merge::prepare_error)?;
    let branch = format!("revert-{number}-{}", uuid::Uuid::new_v4().simple());
    codeyard_ci_engine::refs::create_branch(
        pool,
        ci.cluster.as_ref(),
        ci,
        actor,
        &owner,
        repo.id,
        &branch,
        &sha,
    )
    .await
    .map_err(|error| match error {
        codeyard_repos_core::ref_errors::RefError::Git(error) => {
            crate::repositories::pull_requests::merge::prepare_error(error)
        }
        codeyard_repos_core::ref_errors::RefError::Access(_)
        | codeyard_repos_core::ref_errors::RefError::Protected(_) => {
            OperationError::Denied {
                code: "write_denied",
                message: "Revert branch creation denied".into(),
            }
        }
        codeyard_repos_core::ref_errors::RefError::Invalid(message) => {
            OperationError::Invalid {
                code: "validation_error",
                message: message.into(),
            }
        }
        _ => OperationError::Unavailable {
            code: "storage_unavailable",
            message: format!(
                "Revert branch publication was not confirmed; check {branch} before retrying"
            ),
        },
    })?;
    crate::repositories::pull_requests::mutations::create_pull_request(pool, ci, broadcaster, actor, owner, name,
        crate::repositories::pull_requests::mutations::CreatePullRequestRequest {
            suppress_ci: false,
            head_repo_id: None,
            source: crate::repositories::pull_requests::mutations::PullRequestSource::New { title, body: Some(body) },
            head_branch: branch.clone(),
            base_branch,
            is_draft: request.is_draft,
            maintainer_can_modify: Some(false),
        }).await.map_err(|error| OperationError::Unavailable {
            code: "revert_branch_created",
            message: format!("Revert branch {branch} was created, but opening its pull request was not confirmed: {error}"),
        })
}
