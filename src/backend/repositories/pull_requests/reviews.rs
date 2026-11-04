use crate::backend::cluster::NodeContext;
use crate::backend::repositories::pull_requests::database_error;
use crate::backend::repositories::pull_requests::OperationError;

use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::notify_mentions;
use crate::backend::notifications::mentions::MentionSource;
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::store::repo_store;

use crate::backend::webhooks::payloads::Scope;
use codeyard_git::error::ErrorKind;
use serde::Deserialize;
use serde_json::json;
#[derive(Deserialize)]
pub struct CreatePullRequestReviewRequest {
    pub state: Option<String>,
    pub body: Option<String>,
    pub commit_sha: Option<String>,
    /// Submitting a particular draft must not publish a different, concurrently created review.
    pub review_id: Option<i64>,
    #[serde(skip)]
    pub token: Option<i64>,
}

pub struct CreatePullRequestReviewBatchRequest {
    pub review: CreatePullRequestReviewRequest,
    pub comments:
        Vec<crate::backend::repositories::pull_requests::comments::CreatePullRequestCommentRequest>,
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
    pub target: ReviewerTarget,
    pub created: bool,
}

/// Edit a review through the same author check, native transaction and edited webhook as REST.
/// Names that the edit adds to a published review are notified.
pub async fn edit_review(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    repo: &codeyard_db_core::models::Repository,
    pull: &codeyard_db_core::models::PullRequest,
    id: i64,
    body: &str,
) -> Result<codeyard_db_core::models::PullRequestReview, OperationError> {
    let actor = crate::backend::repositories::pull_requests::actor(pool, actor_id).await?;
    crate::backend::repositories::pull_requests::require_read(pool, &actor, repo).await?;
    if pull.repo_id != repo.id {
        return Err(OperationError::Missing {
            code: "not_found",
            message: "Review not found".into(),
        });
    }
    check_optional_text(Some(body))?;
    let previous =
        codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(pool, id)
            .await
            .map_err(database_error)?
            .filter(|r| r.pull_request_id == pull.id)
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Review not found".into(),
            })?;
    if previous.author_id != actor.id {
        return Err(if previous.state == "pending" {
            OperationError::Missing {
                code: "not_found",
                message: "Review not found".into(),
            }
        } else {
            OperationError::Denied {
                code: "write_denied",
                message: "Only the review author may edit it".into(),
            }
        });
    }
    let review = codeyard_db_repos::pull_requests::reviews::edit_review_body(
        pool, pull.id, actor.id, id, body,
    )
    .await
    .map_err(crate::backend::repositories::pull_requests::write_error)?
    .ok_or_else(|| OperationError::Missing {
        code: "not_found",
        message: "Review not found".into(),
    })?;
    if review.state != "pending" {
        // The edit is saved; an unreadable owner name only skips the mention notices.
        if let Ok(Some(owner)) =
            codeyard_db_repos::repositories::records::repository_owner_name(pool, repo.id).await
        {
            let link = format!(
                "/{owner}/{}/pulls/{}?review={2}#review-{2}",
                repo.name, pull.number, review.id
            );
            notify_mentions(
                pool,
                broadcaster,
                MentionSource {
                    actor: &actor,
                    repository: repo,
                    owner: &owner,
                    repo: &repo.name,
                    number: pull.number,
                    subject: &pull.title,
                    link: &link,
                    comment_id: None,
                    old_text: Some(previous.body.as_deref().unwrap_or("")),
                    text: review.body.as_deref().unwrap_or(""),
                    notified: &[],
                },
            )
            .await;
        }
        emit_review_webhook(pool, repo, pull, &review, "edited", actor.id).await;
    }
    Ok(review)
}

/// Sends one `pull_request_review` webhook about `review`.
async fn emit_review_webhook(
    pool: &codeyard_db_core::DbPool,
    repository: &codeyard_db_core::models::Repository,
    pull: &codeyard_db_core::models::PullRequest,
    review: &codeyard_db_core::models::PullRequestReview,
    action: &'static str,
    sender_id: i64,
) {
    let (review_for_hook, pull_for_hook, repository_for_hook) =
        (review.clone(), pull.clone(), repository.clone());
    let (pull_author_id, reviewer_id) = (pull.author_id, review.author_id);
    crate::backend::webhooks::emit(
        pool,
        repository,
        codeyard_model::webhook_events::PULL_REQUEST_REVIEW,
        vec![pull_author_id, reviewer_id, sender_id],
        move |owner, people| {
            crate::backend::webhooks::payloads::pull_request_review(
                action,
                &review_for_hook,
                &pull_for_hook,
                &people.get(pull_author_id),
                &people.get(reviewer_id),
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &people.get(sender_id),
                },
            )
        },
    )
    .await;
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum ReviewerTarget {
    User(codeyard_db_core::models::User),
    Team(codeyard_db_core::models::Team),
}

/// Review bodies and dismissal reasons use the discussion text limit. Leaving one
/// out, or sending an empty one, is fine.
fn check_optional_text(text: Option<&str>) -> Result<(), OperationError> {
    codeyard_model::issue_input::validate_body(text.unwrap_or(""), false).map_err(|message| {
        OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        }
    })
}

/// Native forms bind both immutable identities and the pending draft, including its absence.
pub struct ReviewTarget {
    pub owner: String,
    pub name: String,
    pub number: i32,
    pub expected: Option<(i64, i64, Option<i64>)>,
}

impl From<(String, String, i32)> for ReviewTarget {
    fn from((owner, name, number): (String, String, i32)) -> Self {
        Self {
            owner,
            name,
            number,
            expected: None,
        }
    }
}

pub async fn create_pull_request_review(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    target: impl Into<ReviewTarget>,
    req: CreatePullRequestReviewRequest,
) -> Result<codeyard_db_core::models::PullRequestReview, OperationError> {
    create_review(
        pool,
        cluster,
        broadcaster,
        actor_id,
        credential,
        target.into(),
        req,
        None,
    )
    .await
}

pub async fn create_pull_request_review_batch(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    target: impl Into<ReviewTarget>,
    batch: CreatePullRequestReviewBatchRequest,
) -> Result<codeyard_db_core::models::PullRequestReview, OperationError> {
    create_review(
        pool,
        cluster,
        broadcaster,
        actor_id,
        credential,
        target.into(),
        batch.review,
        Some(batch.comments),
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
async fn create_review(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    target: ReviewTarget,
    req: CreatePullRequestReviewRequest,
    batch: Option<
        Vec<crate::backend::repositories::pull_requests::comments::CreatePullRequestCommentRequest>,
    >,
) -> Result<codeyard_db_core::models::PullRequestReview, OperationError> {
    let ReviewTarget {
        owner,
        name: repo,
        number,
        expected,
    } = target;
    let user = crate::backend::repositories::pull_requests::actor(pool, actor_id).await?;

    let repository = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    let review_state = req
        .state
        .as_deref()
        .map(str::to_lowercase)
        .unwrap_or_else(|| "commented".to_string());

    // As on GitHub, anyone who can read the repository may review; only writers' approvals
    // count toward the merge rules (see `eligible_reviews`).
    crate::backend::repositories::pull_requests::require_read(pool, &user, &repository).await?;

    let pull_request =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;

    if expected.is_some_and(|(repository_id, pull_id, _)| {
        repository_id != repository.id || pull_id != pull_request.id
    }) {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message: "The repository or pull request changed. Reload before submitting.".to_owned(),
        });
    }
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

    let draft_commit = if let Some(id) = req.review_id {
        codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(pool, id)
            .await
            .map_err(database_error)?
            .filter(|review| {
                review.pull_request_id == pull_request.id
                    && review.author_id == user.id
                    && review.state == "pending"
            })
            .and_then(|review| review.commit_sha)
    } else {
        None
    };
    let mut commit_sha = req
        .commit_sha
        .as_deref()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .map(str::to_string)
        .or_else(|| draft_commit.clone())
        .unwrap_or_else(|| pull_request.head_sha.clone());

    if !codeyard_git::is_full_commit_id(&commit_sha) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "commit_sha must be a 40-character hex SHA-1".to_owned(),
        });
    }
    commit_sha.make_ascii_lowercase();

    // A pending draft submitted by its id may stay on the commit it was started on, as GitHub
    // allows; the database checks that commit again when it saves. Stale-review rules decide
    // whether such an approval counts. Any other review is on the published head.
    let on_draft_commit = draft_commit
        .as_deref()
        .is_some_and(|draft| draft.eq_ignore_ascii_case(&commit_sha));
    if !on_draft_commit {
        let published_ref = if pull_request.head_repo_id == Some(repository.id) {
            format!("refs/heads/{}", pull_request.head_branch)
        } else {
            format!("refs/pull/{}/head", pull_request.number)
        };
        let published = repo_store(&repository, cluster)
            .commit_from_primary(&published_ref)
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
    }

    let create_pending = batch.is_some();
    let comments = batch.unwrap_or_default();
    if comments.len() > codeyard_db_repos::pull_requests::reviews::MAX_REVIEW_COMMENTS as usize
        || comments
            .iter()
            .any(|comment| comment.path.is_none() || comment.in_reply_to_comment_id.is_some())
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "A review holds at most 200 new inline threads.".into(),
        });
    }
    let mut patches = crate::backend::repositories::pull_requests::comments::DiffCache::new();
    let mut prepared = Vec::with_capacity(comments.len());
    for comment in comments {
        prepared.push(
            crate::backend::repositories::pull_requests::comments::prepare_comment(
                pool,
                cluster,
                &repository,
                &pull_request,
                comment,
                &mut patches,
            )
            .await?,
        );
    }
    let comments: Vec<_> = prepared
        .iter()
        .map(|comment| comment.input(&pull_request, user.id))
        .collect();
    let (review, published_ids) = codeyard_db_repos::pull_requests::reviews::save_review_batch(
        pool,
        codeyard_db_repos::pull_requests::reviews::ReviewBatch {
            pr_id: pull_request.id,
            author_id: user.id,
            state: if create_pending && req.state.is_none() {
                None
            } else {
                Some(&review_state)
            },
            body: req.body.as_deref(),
            commit_sha: Some(&commit_sha),
            expected_pending: expected
                .map(|(_, _, pending)| pending)
                .or_else(|| req.review_id.map(Some)),
            create_pending,
            comments: &comments,
            token: req.token,
            credential,
        },
    )
    .await
    .map_err(crate::backend::repositories::pull_requests::write_error)?;
    if review.state == "pending" {
        return Ok(review);
    }

    // The link names the review; the pull request page finds its current page when followed.
    let base = format!("/{}/{}/pulls/{}", owner, repo, pull_request.number);
    let link = format!("{base}?review={0}#review-{0}", review.id);

    let title = format!(
        "{} submitted a {} review",
        user.username,
        review.state.as_str()
    );
    let notice = NotificationRequest {
        user_id: pull_request.author_id,
        actor_id: Some(user.id),
        repo_id: Some(repository.id),
        kind: "pull_request_review",
        title: &title,
        body: review.body.as_deref(),
        link: Some(&link),
        metadata: json!({"repo_owner":owner,"repo_name":repo,"pull_number":pull_request.number,"subject":pull_request.title,"review_id":review.id,"state":review.state.as_str()}),
    };
    let mut notified = Vec::new();
    if pull_request.author_id != user.id
        && create_and_broadcast_notification(pool, broadcaster, notice.clone())
            .await
            .is_ok()
    {
        notified.push(pull_request.author_id);
    }

    // Mentions come before the subscriber notice, so a mentioned subscriber hears them as such.
    let mut mentioned = notify_mentions(
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
            comment_id: None,
            old_text: None,
            text: review.body.as_deref().unwrap_or(""),
            notified: &notified,
        },
    )
    .await;

    // The inline comments this review published were held back, so their mentions go out now.
    // Only the comments the submission itself published: a reply saved after it publishes its own.
    let published = if published_ids.is_empty() {
        Vec::new()
    } else {
        codeyard_db_repos::pull_requests::reviews::list_review_comments(pool, &[review.id])
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(review_id = review.id, %error, "could not notify published review comments");
                Vec::new()
            })
    };
    for entry in published
        .iter()
        .filter(|entry| published_ids.contains(&entry.comment.id))
    {
        let comment_link = format!("{base}?comment={0}#comment-{0}", entry.comment.id);
        let comment_mentioned = notify_mentions(
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
                comment_id: Some(
                    crate::backend::notifications::mentions::MentionComment::PullRequest(
                        entry.comment.id,
                    ),
                ),
                old_text: None,
                text: &entry.comment.body,
                notified: &notified,
            },
        )
        .await;
        mentioned.extend(comment_mentioned);
        crate::backend::repositories::pull_requests::comments::emit_comment_webhook(
            pool,
            &repository,
            &pull_request,
            &entry.comment,
            "created",
            user.id,
        )
        .await;
    }
    notified.extend(mentioned);
    crate::backend::notifications::create::notify_subscribers(pool, broadcaster, notice, &notified)
        .await;

    if review.state != "pending" {
        emit_review_webhook(
            pool,
            &repository,
            &pull_request,
            &review,
            "submitted",
            user.id,
        )
        .await;
    }

    Ok(review)
}

pub async fn dismiss_pull_request_review(
    pool: &codeyard_db_core::DbPool,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    review_id: i64,
    req: DismissPullRequestReviewRequest,
) -> Result<codeyard_db_core::models::PullRequestReview, OperationError> {
    let actor = crate::backend::repositories::pull_requests::actor(pool, actor_id).await?;

    let repository = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    crate::backend::repositories::pull_requests::require_read(pool, &actor, &repository).await?;

    crate::backend::repositories::pull_requests::require_admin(pool, &actor, &repository).await?;
    if repository.owner_kind == "user" && !actor.is_admin && !repository.is_personal_owner(actor.id)
    {
        return Err(OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        });
    }

    let pull_request =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;

    let review =
        codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(pool, review_id)
            .await
            .map_err(database_error)?
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
    check_optional_text(req.reason.as_deref())?;
    // Only votes can be dismissed; comments must not mask an earlier decision.
    if !matches!(review.state.as_str(), "approved" | "changes_requested") {
        return Err(OperationError::Missing {
            code: "not_found",
            message: "Review not found".to_owned(),
        });
    }

    let dismissed = codeyard_db_repos::pull_requests::reviews::dismiss_pull_request_review(
        pool,
        review.id,
        actor.id,
        req.reason.as_deref(),
    )
    .await
    .map_err(crate::backend::repositories::pull_requests::write_error)?;

    emit_review_webhook(
        pool,
        &repository,
        &pull_request,
        &dismissed,
        "dismissed",
        actor.id,
    )
    .await;

    Ok(dismissed)
}

pub async fn request_reviewer(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    req: RequestReviewerRequest,
) -> Result<RequestReviewerResult, OperationError> {
    request_reviewer_bound(
        pool,
        broadcaster,
        actor_id,
        (owner, repo, number),
        req,
        None,
    )
    .await
}

pub async fn request_reviewer_bound(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    (owner, repo, number): (String, String, i32),
    req: RequestReviewerRequest,
    expected_repository: Option<i64>,
) -> Result<RequestReviewerResult, OperationError> {
    let repository = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    check_repository_binding(expected_repository, repository.id)?;
    let requester = crate::backend::repositories::pull_requests::actor(pool, actor_id).await?;
    crate::backend::repositories::pull_requests::require_write(pool, &requester, &repository)
        .await?;
    let pull_request =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;

    if let Some(name) = req
        .username
        .strip_prefix('@')
        .and_then(|name| name.split_once('/'))
    {
        if !name.0.eq_ignore_ascii_case(&owner) {
            return Err(OperationError::Invalid {
                code: "invalid_review_team",
                message: "Review team belongs to another organization".to_owned(),
            });
        }
        let target = codeyard_db_repos::pull_requests::team_reviewers::list(
            pool,
            repository.id,
            None,
            Some(actor_id),
        )
        .await
        .map_err(database_error)?
        .into_iter()
        .map(|access| access.team)
        .find(|team| team.slug.eq_ignore_ascii_case(name.1))
        .ok_or_else(|| OperationError::Invalid {
            code: "invalid_review_team",
            message: "Review team not found or lacks write access".to_owned(),
        })?;
        let changed = crate::backend::repositories::pull_requests::team_reviewers::change(
            pool,
            broadcaster,
            &requester,
            &repository,
            &pull_request,
            &[name.1.to_owned()],
            codeyard_db_repos::pull_requests::team_reviewers::ReviewTeamAction::Request,
        )
        .await?;
        return Ok(RequestReviewerResult {
            target: ReviewerTarget::Team(target),
            created: !changed.is_empty(),
        });
    }
    let target = codeyard_db_identity::users::accounts::get_user_by_username(pool, &req.username)
        .await
        .map_err(database_error)?
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

    Ok(RequestReviewerResult {
        target: ReviewerTarget::User(target),
        created,
    })
}

/// Records that `target` is asked to review `pull`, and tells them unless they asked themselves.
/// Returns whether the request is new. Manually chosen reviewers require writer authority.
pub(super) async fn add_review_request(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    requester: &codeyard_db_core::models::User,
    owner: &str,
    repository: &codeyard_db_core::models::Repository,
    pull_request: &codeyard_db_core::models::PullRequest,
    target: &codeyard_db_core::models::User,
) -> Result<bool, OperationError> {
    let created = codeyard_db_repos::pull_requests::reviews::request_pull_request_reviewer(
        pool,
        pull_request.id,
        requester.id,
        target.id,
    )
    .await
    .map_err(crate::backend::repositories::pull_requests::write_error)?;

    if created {
        notify_review_request(
            pool,
            broadcaster,
            requester,
            owner,
            repository,
            pull_request,
            target,
        )
        .await;
        emit_reviewer_event(
            pool,
            None,
            repository,
            pull_request,
            "review_requested",
            requester.id,
            target,
        )
        .await;
    }
    Ok(created)
}

/// Sends the `pull_request` event for a review request to `target`, or its removal.
pub(super) async fn emit_reviewer_event(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    repository: &codeyard_db_core::models::Repository,
    pull_request: &codeyard_db_core::models::PullRequest,
    action: &'static str,
    sender_id: i64,
    target: &codeyard_db_core::models::User,
) {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "requested_reviewer".into(),
        crate::backend::webhooks::payloads::user(target),
    );
    crate::backend::repositories::pull_requests::emit_pull_request_event(
        pool,
        cluster,
        repository,
        pull_request,
        action,
        sender_id,
        extra,
    )
    .await;
}

pub(super) async fn notify_review_request(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    requester: &codeyard_db_core::models::User,
    owner: &str,
    repository: &codeyard_db_core::models::Repository,
    pull_request: &codeyard_db_core::models::PullRequest,
    target: &codeyard_db_core::models::User,
) {
    if target.id != requester.id {
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
}

fn check_repository_binding(expected: Option<i64>, actual: i64) -> Result<(), OperationError> {
    if expected.is_some_and(|id| id != actual) {
        return Err(OperationError::Conflict {
            code: "repository_changed",
            message: "The repository changed. Reload before changing reviewers.".to_owned(),
        });
    }
    Ok(())
}

pub async fn remove_requested_reviewer(
    pool: &codeyard_db_core::DbPool,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    username: String,
) -> Result<(), OperationError> {
    remove_requested_reviewer_bound(pool, actor_id, (owner, repo, number), username, None).await
}

pub async fn remove_requested_reviewer_bound(
    pool: &codeyard_db_core::DbPool,
    actor_id: i64,
    (owner, repo, number): (String, String, i32),
    username: String,
    expected_repository: Option<i64>,
) -> Result<(), OperationError> {
    let repository = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    check_repository_binding(expected_repository, repository.id)?;
    let actor = crate::backend::repositories::pull_requests::actor(pool, actor_id).await?;
    crate::backend::repositories::pull_requests::require_write(pool, &actor, &repository).await?;
    let pull_request =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;

    if let Some((org, slug)) = username
        .strip_prefix('@')
        .and_then(|name| name.split_once('/'))
    {
        if !org.eq_ignore_ascii_case(&owner) {
            return Err(OperationError::Invalid {
                code: "invalid_review_team",
                message: "Review team belongs to another organization".to_owned(),
            });
        }
        let removed = codeyard_db_repos::pull_requests::team_reviewers::change(
            pool,
            actor.id,
            pull_request.id,
            &[slug.to_owned()],
            codeyard_db_repos::pull_requests::team_reviewers::ReviewTeamAction::Remove,
            None,
        )
        .await
        .map_err(crate::backend::repositories::pull_requests::team_reviewers::error)?;
        crate::backend::repositories::pull_requests::team_reviewers::emit_events(
            pool,
            None,
            &repository,
            &pull_request,
            "review_request_removed",
            actor.id,
            &removed,
        )
        .await;
        return Ok(());
    }
    let target = codeyard_db_identity::users::accounts::get_user_by_username(pool, &username)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "User not found".to_owned(),
        })?;

    let removed = codeyard_db_repos::pull_requests::reviews::remove_pull_request_reviewer(
        pool,
        pull_request.id,
        actor.id,
        target.id,
    )
    .await
    .map_err(crate::backend::repositories::pull_requests::write_error)?;

    if removed {
        emit_reviewer_event(
            pool,
            None,
            &repository,
            &pull_request,
            "review_request_removed",
            actor.id,
            &target,
        )
        .await;
        Ok(())
    } else {
        Err(OperationError::Missing {
            code: "not_found",
            message: "Reviewer request not found".to_owned(),
        })
    }
}

/// Deletes the viewer's pending review and the comments it holds. Nobody else has seen them.
/// Discards the actor's pending review; with `review`, only when that is still the pending one.
pub async fn discard_pending_review(
    pool: &codeyard_db_core::DbPool,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    review: Option<i64>,
) -> Result<(), OperationError> {
    let repository = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    crate::backend::repositories::pull_requests::require_read(
        pool,
        &crate::backend::repositories::pull_requests::actor(pool, actor_id).await?,
        &repository,
    )
    .await?;
    let pull_request =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;

    let discarded = match review {
        Some(review) => codeyard_db_repos::pull_requests::reviews::delete_pending_review_by_id(
            pool,
            pull_request.id,
            actor_id,
            review,
        )
        .await
        .map_err(crate::backend::repositories::pull_requests::write_error)?
        .is_some(),
        None => codeyard_db_repos::pull_requests::reviews::discard_pending_review(
            pool,
            pull_request.id,
            actor_id,
        )
        .await
        .map_err(crate::backend::repositories::pull_requests::write_error)?,
    };
    if discarded {
        Ok(())
    } else {
        Err(OperationError::Missing {
            code: "not_found",
            message: "No pending review to discard".to_owned(),
        })
    }
}
