use super::OperationError;
use crate::backend::cluster::NodeContext;
use crate::backend::db;
use crate::backend::db::pull_requests::reviews::DiscussionRef;
use crate::backend::issues::{discussion_link, discussion_page};
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::{notify_mentions, MentionSource};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::store::{repo_store, ErrorKind};
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use serde::Deserialize;
use serde_json::json;
#[derive(Deserialize)]
pub struct CreatePullRequestReviewRequest {
    pub state: Option<String>,
    pub body: Option<String>,
    pub commit_sha: Option<String>,
}

#[derive(Deserialize)]
pub struct DismissPullRequestReviewRequest {
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct RequestReviewerRequest {
    pub username: String,
}

pub struct RequestReviewerResult {
    pub target: db::models::User,
    pub created: bool,
}

fn is_valid_sha1_hex(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Review bodies and dismissal reasons use the discussion text limit. Leaving one
/// out, or sending an empty one, is fine.
fn check_optional_text(text: Option<&str>) -> Result<(), OperationError> {
    crate::backend::issues::validate_body(text.unwrap_or(""), false).map_err(|message| {
        OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        }
    })
}

pub async fn create_pull_request_review(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    repository: (String, String, i32),
    req: CreatePullRequestReviewRequest,
) -> Result<db::models::PullRequestReview, OperationError> {
    let (owner, repo, number) = repository;
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

    let review_state = req
        .state
        .as_deref()
        .map(str::to_lowercase)
        .unwrap_or_else(|| "commented".to_string());

    // A plain comment needs read access; a vote needs write access. Access is checked before
    // the state is validated, so a user without write access gets the same refusal as before.
    if review_state == "commented" {
        super::require_read(pool, &user, &repository).await?;
    } else {
        super::require_write(pool, &user, &repository).await?;
    }

    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    if pull_request.state != "open" {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Pull request is not open".to_owned(),
        });
    }

    if review_state != "approved"
        && review_state != "changes_requested"
        && review_state != "commented"
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "state must be one of: approved, changes_requested, commented".to_owned(),
        });
    }
    // Approvals may stand alone. Comments and change requests must say why, in the summary or in the
    // inline comments of the pending review that this review submits.
    if review_state != "approved"
        && req.body.as_deref().unwrap_or("").trim().is_empty()
        && !has_pending_comments(pool, pull_request.id, user.id).await?
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "A review body is required unless the review is approved".to_owned(),
        });
    }
    check_optional_text(req.body.as_deref())?;

    if review_state == "approved" && pull_request.author_id == user.id {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Authors cannot approve their own pull request".to_owned(),
        });
    }
    if review_state == "changes_requested" && pull_request.author_id == user.id {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Authors cannot request changes on their own pull request".to_owned(),
        });
    }

    let mut commit_sha = req
        .commit_sha
        .as_deref()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| pull_request.head_sha.clone());

    if !is_valid_sha1_hex(&commit_sha) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "commit_sha must be a 40-character hex SHA-1".to_owned(),
        });
    }
    commit_sha.make_ascii_lowercase();

    let published_ref = if pull_request.head_repo_id == Some(repository.id) {
        format!("refs/heads/{}", pull_request.head_branch)
    } else {
        format!("refs/pull/{}/head", pull_request.number)
    };
    let published = repo_store(&repository, cluster)
        .commit(&published_ref)
        .await
        .map_err(|error| {
            if error.kind == ErrorKind::Unavailable {
                OperationError::Unavailable {
                    code: "git_unavailable",
                    message: "The published pull request head is temporarily unavailable"
                        .to_owned(),
                }
            } else {
                OperationError::Conflict {
                    code: "revision_conflict",
                    message: "Review the currently published pull request head".to_owned(),
                }
            }
        })?;
    if published.sha != commit_sha {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message: "Review the currently published pull request head".to_owned(),
        });
    }

    let review = db::pull_requests::reviews::add_pull_request_review(
        pool,
        pull_request.id,
        user.id,
        &review_state,
        req.body.as_deref(),
        Some(commit_sha.as_str()),
    )
    .await
    .map_err(super::write_error)?;

    // The review sits at its own place in the Conversation, and its notices link to that place.
    let base = format!("/{}/{}/pulls/{}", owner, repo, pull_request.number);
    let page = discussion_page(
        db::pull_requests::reviews::count_discussion_items_through(
            pool,
            pull_request.id,
            DiscussionRef::Review(review.id),
        )
        .await,
    );
    let link = discussion_link(&base, page, &format!("review-{}", review.id));

    if pull_request.author_id != user.id {
        let title = format!(
            "{} submitted a {} review",
            user.username,
            review.state.as_str()
        );
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: pull_request.author_id,
                actor_id: Some(user.id),
                repo_id: Some(repository.id),
                kind: "pull_request_review",
                title: &title,
                body: review.body.as_deref(),
                link: Some(&link),
                metadata: json!({
                    "repo_owner": owner,
                    "repo_name": repo,
                    "pull_number": pull_request.number,
                    "subject": pull_request.title,
                    "review_id": review.id,
                    "state": review.state.as_str(),
                }),
            },
        )
        .await;
    }

    notify_mentions(
        pool,
        broadcaster,
        MentionSource {
            actor: &user,
            repository: &repository,
            owner: &owner,
            repo: &repo,
            number: pull_request.number,
            subject: &pull_request.title,
            link: &link,
            old_text: None,
            text: review.body.as_deref().unwrap_or(""),
            notified: &[pull_request.author_id],
        },
    )
    .await;

    // The inline comments this review published were held back, so their mentions go out now.
    let published = db::pull_requests::reviews::list_review_comments(pool, &[review.id])
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?;
    for entry in &published {
        let comment_link = discussion_link(&base, page, &format!("comment-{}", entry.comment.id));
        notify_mentions(
            pool,
            broadcaster,
            MentionSource {
                actor: &user,
                repository: &repository,
                owner: &owner,
                repo: &repo,
                number: pull_request.number,
                subject: &pull_request.title,
                link: &comment_link,
                old_text: None,
                text: &entry.comment.body,
                notified: &[pull_request.author_id],
            },
        )
        .await;
    }

    if review.state != "pending" {
        let (review_for_hook, pull_for_hook, repository_for_hook) =
            (review.clone(), pull_request.clone(), repository.clone());
        let (pull_author_id, reviewer_id, sender_id) =
            (pull_request.author_id, review.author_id, user.id);
        webhooks::emit(
            pool,
            &repository,
            events::PULL_REQUEST_REVIEW,
            vec![pull_author_id, reviewer_id, sender_id],
            move |owner, people| {
                let (pull_author, reviewer, sender) = (
                    people.get(pull_author_id),
                    people.get(reviewer_id),
                    people.get(sender_id),
                );
                payloads::pull_request_review(
                    "submitted",
                    &review_for_hook,
                    &pull_for_hook,
                    &pull_author,
                    &reviewer,
                    Scope {
                        repo: &repository_for_hook,
                        owner,
                        sender: &sender,
                    },
                )
            },
        );
    }

    Ok(review)
}

/// Whether the author has inline comments waiting in a pending review.
async fn has_pending_comments(
    pool: &db::DbPool,
    pr_id: i64,
    author_id: i64,
) -> Result<bool, OperationError> {
    db::pull_requests::reviews::list_pending_review_comments(pool, pr_id, author_id)
        .await
        .map(|comments| !comments.is_empty())
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })
}

pub async fn dismiss_pull_request_review(
    pool: &db::DbPool,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    review_id: i64,
    req: DismissPullRequestReviewRequest,
) -> Result<db::models::PullRequestReview, OperationError> {
    let actor = super::actor(pool, actor_id).await?;

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

    super::require_read(pool, &super::actor(pool, actor_id).await?, &repository).await?;

    super::require_admin(pool, &super::actor(pool, actor_id).await?, &repository).await?;
    if repository.owner_kind == "user" && !actor.is_admin && !repository.is_personal_owner(actor.id)
    {
        return Err(OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        });
    }

    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    let review = db::pull_requests::reviews::get_pull_request_review_by_id(pool, review_id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Review not found".to_owned(),
        })?;

    if review.pull_request_id != pull_request.id {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Review does not belong to this pull request".to_owned(),
        });
    }
    // A pending review is private to its author until it is submitted, so it has no dismissal.
    if review.state == "pending" {
        return Err(OperationError::Missing {
            code: "not_found",
            message: "Review not found".to_owned(),
        });
    }

    check_optional_text(req.reason.as_deref())?;
    let dismissed = db::pull_requests::reviews::dismiss_pull_request_review(
        pool,
        review.id,
        actor.id,
        req.reason.as_deref(),
    )
    .await
    .map_err(super::write_error)?;

    let (review_for_hook, pull_for_hook, repository_for_hook) =
        (dismissed.clone(), pull_request.clone(), repository.clone());
    let (pull_author_id, reviewer_id, sender_id) =
        (pull_request.author_id, dismissed.author_id, actor.id);
    webhooks::emit(
        pool,
        &repository,
        events::PULL_REQUEST_REVIEW,
        vec![pull_author_id, reviewer_id, sender_id],
        move |owner, people| {
            let (pull_author, reviewer, sender) = (
                people.get(pull_author_id),
                people.get(reviewer_id),
                people.get(sender_id),
            );
            payloads::pull_request_review(
                "dismissed",
                &review_for_hook,
                &pull_for_hook,
                &pull_author,
                &reviewer,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );

    Ok(dismissed)
}

pub async fn request_reviewer(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    req: RequestReviewerRequest,
) -> Result<RequestReviewerResult, OperationError> {
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

    super::require_write(pool, &super::actor(pool, actor_id).await?, &repository).await?;
    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    let target = db::users::accounts::get_user_by_username(pool, &req.username)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "User not found".to_owned(),
        })?;

    if target.id == pull_request.author_id {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Cannot request review from pull request author".to_owned(),
        });
    }

    let requester = super::actor(pool, actor_id).await?;
    let created = add_review_request(
        pool,
        broadcaster,
        &requester,
        &owner,
        &repository,
        &pull_request,
        &target,
    )
    .await?;

    Ok(RequestReviewerResult { target, created })
}

/// Records that `target` is asked to review `pull`, and tells them unless they asked themselves.
/// Returns whether the request is new. Shared by people's requests and by code owner requests.
pub(super) async fn add_review_request(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    requester: &db::models::User,
    owner: &str,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    target: &db::models::User,
) -> Result<bool, OperationError> {
    let created = db::pull_requests::reviews::request_pull_request_reviewer(
        pool,
        pull_request.id,
        requester.id,
        target.id,
    )
    .await
    .map_err(super::write_error)?;

    if created && target.id != requester.id {
        let title = format!(
            "{} requested your review on pull request #{}",
            requester.username, pull_request.number
        );
        let link = format!(
            "/{}/{}/pulls/{}",
            owner, repository.name, pull_request.number
        );
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: target.id,
                actor_id: Some(requester.id),
                repo_id: Some(repository.id),
                kind: "pull_request_review_requested",
                title: &title,
                body: Some(&pull_request.title),
                link: Some(&link),
                metadata: json!({
                    "repo_owner": owner,
                    "repo_name": repository.name,
                    "pull_number": pull_request.number,
                    "subject": pull_request.title,
                }),
            },
        )
        .await;
    }
    Ok(created)
}

pub async fn remove_requested_reviewer(
    pool: &db::DbPool,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    username: String,
) -> Result<(), OperationError> {
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

    super::require_write(pool, &super::actor(pool, actor_id).await?, &repository).await?;
    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    let target = db::users::accounts::get_user_by_username(pool, &username)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "User not found".to_owned(),
        })?;

    let actor = super::actor(pool, actor_id).await?;
    let removed = db::pull_requests::reviews::remove_pull_request_reviewer(
        pool,
        pull_request.id,
        actor.id,
        target.id,
    )
    .await
    .map_err(super::write_error)?;

    if removed {
        Ok(())
    } else {
        Err(OperationError::Missing {
            code: "not_found",
            message: "Reviewer request not found".to_owned(),
        })
    }
}

/// Deletes the viewer's pending review and the comments it holds. Nobody else has seen them.
pub async fn discard_pending_review(
    pool: &db::DbPool,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
) -> Result<(), OperationError> {
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

    super::require_read(pool, &super::actor(pool, actor_id).await?, &repository).await?;
    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    let discarded =
        db::pull_requests::reviews::discard_pending_review(pool, pull_request.id, actor_id)
            .await
            .map_err(super::write_error)?;
    if discarded {
        Ok(())
    } else {
        Err(OperationError::Missing {
            code: "not_found",
            message: "No pending review to discard".to_owned(),
        })
    }
}
