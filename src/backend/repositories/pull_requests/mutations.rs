use super::OperationError;
use crate::backend::ci::CiRuns;
use crate::backend::db;
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::{notify_mentions, MentionSource};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::pull_requests::queue_pull_request_ci;
use crate::backend::repositories::store::{repo_store, ErrorKind, RepoError};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
#[derive(Deserialize)]
pub struct CreatePullRequestRequest {
    pub head_repo_id: Option<i64>,
    pub title: String,
    pub body: Option<String>,
    pub head_branch: String,
    pub base_branch: String,
    #[serde(default)]
    pub is_draft: bool,
}

#[derive(Deserialize)]
pub struct UpdatePullRequestRequest {
    pub expected_state: String,
    pub expected_updated: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    /// A new base branch for an open pull request.
    pub base: Option<String>,
}

/// Branch names are stored in VARCHAR(255) columns, which count characters.
fn check_branch_name(branch: &str) -> Result<(), OperationError> {
    if crate::backend::git::branch_ref(branch).is_none() || branch.chars().count() > 255 {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Branch names must be valid Git branch names of at most 255 characters."
                .to_owned(),
        });
    }
    Ok(())
}

/// The pull request row is written before its fork head is published. A refused head is a
/// request error. Any other failure leaves the opened pull request without its head.
fn head_publish_error(number: i32, error: RepoError) -> OperationError {
    if error.kind == ErrorKind::Invalid {
        OperationError::Invalid {
            code: "validation_error",
            message: error.message,
        }
    } else {
        OperationError::Opened { number }
    }
}

pub async fn create_pull_request(
    pool: &db::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    owner: String,
    repo: String,
    req: CreatePullRequestRequest,
) -> Result<db::models::PullRequest, OperationError> {
    let user = super::actor(pool, actor_id).await?;
    let repository = db::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    super::require_read(pool, &user, &repository).await?;
    let checked = crate::backend::issues::validate_title(&req.title).and_then(|_| {
        crate::backend::issues::validate_body(req.body.as_deref().unwrap_or(""), false)
    });
    if let Err(message) = checked {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }
    for branch in [&req.head_branch, &req.base_branch] {
        check_branch_name(branch)?;
    }
    let head_repository = crate::backend::repositories::pull_requests::head_repository(
        pool,
        &repository,
        req.head_repo_id.unwrap_or(repository.id),
    )
    .await
    .map_err(|error| match error {
        super::branches::HeadError::Unavailable => OperationError::Unavailable {
            code: "database_error",
            message: "Head repository lookup is unavailable".into(),
        },
        super::branches::HeadError::Missing | super::branches::HeadError::OutsideFork => {
            OperationError::Invalid {
                code: "invalid_head",
                message: "Head must be this repository or one of its forks".into(),
            }
        }
    })?;
    super::require_read(pool, &user, &head_repository).await?;
    if head_repository.id == repository.id && req.head_branch == req.base_branch {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Head and base branch cannot be the same".to_owned(),
        });
    }

    let refs: HashMap<String, String> = repo_store(&repository, ci.cluster.as_ref())
        .refs()
        .await
        .map_err(|_| OperationError::Unavailable {
        code: "git_unavailable",
        message: "Repository storage is unavailable".into(),
    })?;
    let head_ref = format!("refs/heads/{}", req.head_branch);
    let head_refs = if head_repository.id == repository.id {
        refs.clone()
    } else {
        repo_store(&head_repository, ci.cluster.as_ref())
            .refs()
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "git_unavailable",
                message: "Head repository storage is unavailable".into(),
            })?
    };
    let head_sha = head_refs
        .get(&head_ref)
        .cloned()
        .ok_or_else(|| OperationError::Invalid {
            code: "git_error",
            message: format!("Head branch not found: {}", req.head_branch),
        })?;
    let base_ref = format!("refs/heads/{}", req.base_branch);
    let base_sha = refs
        .get(&base_ref)
        .cloned()
        .ok_or_else(|| OperationError::Invalid {
            code: "git_error",
            message: format!("Base branch not found: {}", req.base_branch),
        })?;

    let created = db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
            repo_id: repository.id,
            head_repo_id: Some(head_repository.id),
            author_id: user.id,
            title: &req.title,
            body: req.body.as_deref().unwrap_or(""),
            head_branch: &req.head_branch,
            head_sha: &head_sha,
            base_branch: &req.base_branch,
            base_sha: &base_sha,
            is_draft: req.is_draft,
        },
    )
    .await;
    let pull_request = match created {
        Ok(pull_request) => pull_request,
        Err(error) => {
            let number =
                match error.downcast::<db::pull_requests::records::DuplicateOpenPullRequest>() {
                    Ok(duplicate) => duplicate.number,
                    Err(error) => return Err(super::write_error(error)),
                };
            let existing =
                db::pull_requests::records::get_pull_request(pool, repository.id, number)
                    .await
                    .map_err(|_| OperationError::Unavailable {
                        code: "database_error",
                        message: "Collaboration storage is temporarily unavailable".to_owned(),
                    })?;
            let Some(existing) = existing.filter(|pull| pull.state == "open") else {
                return Err(OperationError::Conflict {
                    code: "revision_conflict",
                    message: "Pull request state changed; retry the request".to_owned(),
                });
            };
            if head_repository.id != repository.id {
                // A retry after interruption repairs only an absent initial
                // pull ref. Exact CAS cannot overwrite a later publication.
                repo_store(&repository, ci.cluster.as_ref())
                    .fetch_pull_head(
                        pool,
                        repository.id,
                        head_repository.id,
                        existing.author_id,
                        &repo_store(&head_repository, ci.cluster.as_ref()),
                        existing.number,
                        &existing.head_sha,
                        None,
                    )
                    .await
                    .map_err(|error| head_publish_error(existing.number, error))?;
            }
            return Err(OperationError::Conflict {
                code: "pull_request_exists",
                message: format!(
                    "An open pull request already exists for these branches as #{number}"
                ),
            });
        }
    };

    if head_repository.id != repository.id {
        repo_store(&repository, ci.cluster.as_ref())
            .fetch_pull_head(
                pool,
                repository.id,
                head_repository.id,
                pull_request.author_id,
                &repo_store(&head_repository, ci.cluster.as_ref()),
                pull_request.number,
                &pull_request.head_sha,
                None,
            )
            .await
            .map_err(|error| head_publish_error(pull_request.number, error))?;
    }

    if let Some(owner_user_id) = repository.personal_owner_id().filter(|id| *id != user.id) {
        let title = format!(
            "{} opened pull request #{}",
            user.username, pull_request.number
        );
        let link = format!("/{}/{}/pulls/{}", owner, repo, pull_request.number);
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: owner_user_id,
                actor_id: Some(user.id),
                repo_id: Some(repository.id),
                kind: "pull_request_opened",
                title: &title,
                body: Some(&pull_request.title),
                link: Some(&link),
                metadata: json!({
                    "repo_owner": owner,
                    "repo_name": repo,
                    "pull_number": pull_request.number,
                    "head_branch": pull_request.head_branch.as_str(),
                    "base_branch": pull_request.base_branch.as_str(),
                }),
            },
        )
        .await;
    }

    let link = format!("/{}/{}/pulls/{}", owner, repo, pull_request.number);
    notify_mentions(
        pool,
        broadcaster,
        MentionSource {
            actor: &user,
            repository: &repository,
            owner: &owner,
            repo: &repo,
            number: pull_request.number,
            link: &link,
            old_text: None,
            text: &pull_request.body,
            notified: &repository
                .personal_owner_id()
                .into_iter()
                .collect::<Vec<_>>(),
        },
    )
    .await;

    if !pull_request.is_draft {
        let _ = queue_pull_request_ci(pool, ci, &repository, &pull_request, user.id).await;
    }

    Ok(pull_request)
}

pub async fn update_pull_request(
    pool: &db::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    (owner, repo, number): (String, String, i32),
    req: UpdatePullRequestRequest,
) -> Result<db::models::PullRequest, OperationError> {
    let repository = db::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    let user = super::actor(pool, actor_id).await?;
    super::require_write(pool, &user, &repository).await?;
    let existing = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    let requested_state = match req.state.as_deref() {
        Some("open") | Some("closed") => req.state.as_deref(),
        Some("merged") => {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "Use merge endpoint to merge pull requests".to_owned(),
            })
        }
        Some(_) => {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "state must be one of: open, closed".to_owned(),
            })
        }
        None => None,
    };

    if existing.state == "merged" && requested_state.is_some() {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Merged pull requests cannot be reopened or closed".to_owned(),
        });
    }

    let expected_updated = req
        .expected_updated
        .parse::<chrono::DateTime<chrono::Utc>>()
        .map_err(|_| OperationError::Invalid {
            code: "validation_error",
            message: "expected_updated must be the timestamp returned when reading this entry"
                .to_owned(),
        })?;
    if req
        .title
        .as_ref()
        .is_some_and(|title| crate::backend::issues::validate_title(title).is_err())
        || req
            .body
            .as_ref()
            .is_some_and(|body| crate::backend::issues::validate_body(body, false).is_err())
        || !matches!(req.expected_state.as_str(), "open" | "closed" | "merged")
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Invalid title, body, or expected state".to_owned(),
        });
    }
    if req.base.is_some() && existing.stack_id.is_some() {
        return Err(OperationError::Conflict {
            code: "conflict",
            message: "Unstack the pull request before changing its base branch.".to_owned(),
        });
    }
    let base_commit = match req.base.as_deref() {
        Some(branch) => Some((
            branch.to_owned(),
            base_branch_commit(ci, &repository, &existing, branch).await?,
        )),
        None => None,
    };
    let base_changed = base_commit.is_some();
    let new_base = base_commit
        .as_ref()
        .map(|(branch, sha)| db::pull_requests::records::NewBase {
            branch: branch.as_str(),
            sha: sha.as_str(),
        });
    let pull_request = db::pull_requests::records::update_pull_request(
        pool,
        existing.id,
        db::discussions::DiscussionEdit {
            actor_id: user.id,
            expected_state: &req.expected_state,
            expected_updated,
            title: req.title.as_deref(),
            body: req.body.as_deref(),
            state: requested_state,
        },
        new_base,
    ).await.map_err(|error| match error.downcast::<db::pull_requests::records::DuplicateOpenPullRequest>() {
        Ok(duplicate) => OperationError::Conflict {
            code: "conflict",
            message: format!("Pull request #{} is already open for these branches", duplicate.number),
        },
        Err(_) => {
            tracing::error!(pull_request_id = existing.id, "conditional pull request update failed");
            OperationError::Unavailable { code: "database_error", message: "Unable to save this entry; your submission was not confirmed".to_owned() }
        }
    })?.ok_or_else(|| OperationError::Conflict { code: "conflict", message: "This entry, merge operation, or your permission changed; reload and review your preserved submission".to_owned() })?;

    if let Some(body) = req.body.as_deref() {
        let link = format!("/{owner}/{repo}/pulls/{number}");
        notify_mentions(
            pool,
            broadcaster,
            MentionSource {
                actor: &user,
                repository: &repository,
                owner: &owner,
                repo: &repo,
                number,
                link: &link,
                old_text: Some(&existing.body),
                text: body,
                notified: &[],
            },
        )
        .await;
    }

    // Branches may have moved while the pull request was closed. Catch up now that
    // it is open again, and answer with what it is after that.
    let pull_request = if requested_state == Some("open") || base_changed {
        super::branches::refresh_open_pull_request(pool, &repository, ci, user.id, pull_request.id)
            .await
            .map_err(|_| OperationError::Updated { number })?;
        db::pull_requests::records::get_pull_request_by_id(pool, pull_request.id)
            .await
            .map_err(|_| OperationError::Updated { number })?
            .ok_or(OperationError::Updated { number })?
    } else {
        pull_request
    };
    if base_changed {
        // A new base can make workflows eligible that the old one did not, so queue them for it.
        // The change is already saved, so the wait for them is bounded.
        super::workflows::queue_moved_pull_requests_ci(
            pool,
            ci,
            &repository,
            std::slice::from_ref(&pull_request),
            user.id,
        )
        .await;
    }

    Ok(pull_request)
}

/// The commit that a new base branch of an open pull request points at. Only an open pull request
/// can change its base, and only to a branch of this repository other than its own head branch.
async fn base_branch_commit(
    ci: &CiRuns,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    branch: &str,
) -> Result<String, OperationError> {
    if pull_request.state != "open" {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Merged and closed pull requests cannot change their base branch".to_owned(),
        });
    }
    check_branch_name(branch)?;
    if pull_request.head_repo_id == Some(repository.id) && pull_request.head_branch == branch {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Head and base branch cannot be the same".to_owned(),
        });
    }
    let refs = repo_store(repository, ci.cluster.as_ref())
        .refs()
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "git_unavailable",
            message: "Repository storage is unavailable".into(),
        })?;
    refs.get(&format!("refs/heads/{branch}"))
        .cloned()
        .ok_or_else(|| OperationError::Invalid {
            code: "validation_error",
            message: format!("Base branch `{branch}` does not exist."),
        })
}

/// The repository, the actor and the pull request a stack change works on. Only people who can
/// write may change stacks, and a missing pull request is refused before anything changes.
async fn stack_change_target(
    pool: &db::DbPool,
    actor_id: i64,
    owner: &str,
    repo: &str,
    number: i32,
) -> Result<
    (
        db::models::Repository,
        db::models::User,
        db::models::PullRequest,
    ),
    OperationError,
> {
    let repository = db::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    let user = super::actor(pool, actor_id).await?;
    super::require_write(pool, &user, &repository).await?;
    let existing = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    Ok((repository, user, existing))
}

/// Takes the unmerged pull requests of the stack this pull request is in out of it. Each keeps its
/// base branch, and merged pull requests keep the stack. A pull request in no stack is returned
/// unchanged. The database reads the stack when it writes, not from an earlier read.
pub async fn unstack_pull_request(
    pool: &db::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
) -> Result<db::models::PullRequest, OperationError> {
    let (repository, user, existing) =
        stack_change_target(pool, actor_id, &owner, &repo, number).await?;
    // The stack is read before it is taken apart, so its open layers can be queued afterwards.
    let members = match existing.stack_id {
        Some(stack) => db::pull_requests::records::list_stack_members(pool, repository.id, stack)
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "database_error",
                message: "Collaboration storage is temporarily unavailable".to_owned(),
            })?,
        None => Vec::new(),
    };
    let unstacked = db::pull_requests::records::unstack_pull_requests(pool, user.id, existing.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Unable to save this change; it was not confirmed".to_owned(),
        })?;
    if !unstacked {
        return Err(OperationError::Denied {
            code: "forbidden",
            message: "Only people with write access can unstack pull requests".to_owned(),
        });
    }
    // Each open layer goes back to its own base, so workflows for that base may now apply. They are
    // read again, since a join or a reopen may have changed them since the stack was read.
    let mut open = Vec::new();
    for member in members.iter().filter(|pull| pull.state == "open") {
        if let Ok(Some(pull)) =
            db::pull_requests::records::get_pull_request_by_id(pool, member.id).await
        {
            open.push(pull);
        }
    }
    super::workflows::queue_moved_pull_requests_ci(pool, ci, &repository, &open, user.id).await;
    db::pull_requests::records::get_pull_request_by_id(pool, existing.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })
}

/// Adds a pull request to the stack of the open pull request its base branch names, or starts a
/// stack with that pull request. Nothing else adds a pull request to a stack.
pub async fn join_stack(
    pool: &db::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
) -> Result<db::models::PullRequest, OperationError> {
    let (repository, user, existing) =
        stack_change_target(pool, actor_id, &owner, &repo, number).await?;
    let joined = db::pull_requests::records::join_pull_request_stack(pool, user.id, existing.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Unable to save this change; it was not confirmed".to_owned(),
        })?;
    if let db::pull_requests::records::StackJoin::Refused(reason) = joined {
        return Err(join_refusal(reason));
    }
    let pull = db::pull_requests::records::get_pull_request_by_id(pool, existing.id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    // The layer now merges into its stack's trunk, so workflows for that trunk may now apply.
    super::workflows::queue_moved_pull_requests_ci(
        pool,
        ci,
        &repository,
        std::slice::from_ref(&pull),
        user.id,
    )
    .await;
    Ok(pull)
}

/// The answer for a pull request that cannot join a stack.
fn join_refusal(reason: db::pull_requests::records::StackRefusal) -> OperationError {
    use db::pull_requests::records::{StackRefusal, MAX_STACK_LAYERS};
    match reason {
        StackRefusal::Missing => OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        },
        StackRefusal::Denied => OperationError::Denied {
            code: "forbidden",
            message: "Only people with write access can change stacks".to_owned(),
        },
        StackRefusal::NotOpen => OperationError::Conflict {
            code: "invalid_state",
            message: "Only open pull requests can join a stack.".to_owned(),
        },
        StackRefusal::ForkHead => OperationError::Invalid {
            code: "validation_error",
            message: "Only a pull request from this repository's own branch can join a stack."
                .to_owned(),
        },
        StackRefusal::NoParent => OperationError::Conflict {
            code: "stack_join_refused",
            message: "No open pull request in this repository has this base branch as its head."
                .to_owned(),
        },
        StackRefusal::ParentNotTop => OperationError::Conflict {
            code: "stack_join_refused",
            message: "Something is already stacked on that pull request.".to_owned(),
        },
        StackRefusal::AlreadyStacked => OperationError::Conflict {
            code: "stack_join_refused",
            message: "Unstack this pull request before adding it to another stack.".to_owned(),
        },
        StackRefusal::Cycle => OperationError::Conflict {
            code: "stack_join_refused",
            message: "These pull requests would depend on each other, so they cannot be stacked."
                .to_owned(),
        },
        StackRefusal::TooLarge => OperationError::Conflict {
            code: "stack_join_refused",
            message: format!("A stack can have at most {MAX_STACK_LAYERS} pull requests."),
        },
        StackRefusal::Changed => OperationError::Conflict {
            code: "stack_join_refused",
            message: "The stack changed while this pull request was joining. Reload and try again."
                .to_owned(),
        },
    }
}

pub async fn mark_pull_request_ready(
    pool: &db::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
) -> Result<db::models::PullRequest, OperationError> {
    let user = super::actor(pool, actor_id).await?;
    let repository = db::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    super::require_read(pool, &user, &repository).await?;
    let existing = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    let outcome = db::pull_requests::records::mark_pull_request_ready(pool, existing.id, user.id)
        .await
        .map_err(|_| {
            tracing::error!(
                pull_request_id = existing.id,
                "marking a pull request ready failed"
            );
            OperationError::Unavailable {
                code: "database_error",
                message: "Unable to save this change; it was not confirmed".to_owned(),
            }
        })?;
    let pull_request = match outcome {
        db::pull_requests::records::ReadyOutcome::Ready(pull_request) => *pull_request,
        db::pull_requests::records::ReadyOutcome::Missing => {
            return Err(OperationError::Missing { code: "not_found", message: "Pull request not found".to_owned() })
        }
        db::pull_requests::records::ReadyOutcome::Denied => {
            return Err(OperationError::Denied { code: "forbidden", message: "Only the author and people with write access can mark a pull request ready for review".to_owned() })
        }
        db::pull_requests::records::ReadyOutcome::NotOpen => {
            return Err(OperationError::Invalid { code: "invalid_state", message: "Pull request is not open".to_owned() })
        }
        db::pull_requests::records::ReadyOutcome::NotDraft => {
            return Err(OperationError::Conflict { code: "invalid_state", message: "Pull request is not a draft".to_owned() })
        }
    };

    // Branches may have moved while it was a draft. Catch up, then queue its
    // workflows the way a new pull request does.
    super::branches::refresh_open_pull_request(pool, &repository, ci, user.id, pull_request.id)
        .await
        .map_err(|_| OperationError::Updated { number })?;
    let pull_request = db::pull_requests::records::get_pull_request_by_id(pool, pull_request.id)
        .await
        .map_err(|_| OperationError::Updated { number })?
        .ok_or(OperationError::Updated { number })?;
    let _ = queue_pull_request_ci(pool, ci, &repository, &pull_request, user.id).await;

    Ok(pull_request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_head_is_a_request_error_and_other_failures_are_storage_errors() {
        let refused = RepoError::new(
            ErrorKind::Invalid,
            "A repository holds at most 1 pull request heads",
        );
        assert!(matches!(
            head_publish_error(3, refused),
            OperationError::Invalid { message, .. } if message.contains("pull request heads")
        ));
        let failed = RepoError::new(ErrorKind::Unavailable, "Git did not finish");
        assert!(matches!(
            head_publish_error(3, failed),
            OperationError::Opened { number: 3 }
        ));
    }
}
