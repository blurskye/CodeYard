use super::{comment_link, OperationError};
use crate::backend::db;
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::{
    mentions_anyone, notify_mentions, MentionComment, MentionSource,
};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct CreateCommentRequest {
    pub body: String,
}

fn read_error(_: anyhow::Error) -> OperationError {
    OperationError::Unavailable {
        code: "database_error",
        message: "Collaboration storage is temporarily unavailable".to_owned(),
    }
}

/// The comment a change names: its issue or pull request in the repository, and its id.
pub struct CommentTarget {
    pub owner: String,
    pub repo: String,
    pub number: i32,
    pub comment_id: i64,
}

/// The repository and issue a comment belongs to, when the user may read them.
async fn read_issue(
    pool: &db::DbPool,
    user: &db::models::User,
    owner: &str,
    repo: &str,
    number: i32,
) -> Result<(db::models::Repository, db::models::Issue), OperationError> {
    let repository = db::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(read_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    super::require_read(pool, user, &repository).await?;
    let issue = db::issues::get_issue(pool, repository.id, number)
        .await
        .map_err(read_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Issue not found".to_owned(),
        })?;
    Ok((repository, issue))
}

/// The comment with this id, when it is on the issue.
async fn issue_comment(
    pool: &db::DbPool,
    issue: &db::models::Issue,
    comment_id: i64,
) -> Result<db::models::IssueComment, OperationError> {
    db::issues::get_issue_comment(pool, comment_id)
        .await
        .map_err(read_error)?
        .filter(|comment| comment.issue_id == issue.id)
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Comment not found".to_owned(),
        })
}

/// Why a locked thread or an archived repository refuses a comment change, if it does.
fn refuse_change(
    repository: &db::models::Repository,
    issue: &db::models::Issue,
) -> Option<OperationError> {
    if repository.is_archived {
        Some(OperationError::Denied {
            code: "repository_archived",
            message: "This repository is archived and read-only.".to_owned(),
        })
    } else if issue.locked {
        Some(OperationError::Denied {
            code: "conversation_locked",
            message: "This conversation is locked.".to_owned(),
        })
    } else {
        None
    }
}

pub async fn create_issue_comment(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    req: CreateCommentRequest,
) -> Result<db::models::IssueComment, OperationError> {
    let user = super::actor(pool, actor_id).await?;
    let (repository, issue) = read_issue(pool, &user, &owner, &repo, number).await?;
    if let Some(refusal) = refuse_change(&repository, &issue) {
        return Err(refusal);
    }

    if let Err(message) = super::validate_body(&req.body, true) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }

    let comment = db::issues::add_issue_comment(pool, issue.id, user.id, &req.body)
        .await
        .map_err(super::write_error)?;

    // The comment position is counted only when a notification leads to the comment.
    let notifies_author = issue.author_id != user.id;
    if notifies_author || mentions_anyone(&req.body) {
        let base = format!("/{}/{}/issues/{}", owner, repo, issue.number);
        let link = comment_link(
            &base,
            comment.id,
            db::issues::issue_comment_position(pool, comment.id).await,
        );
        if notifies_author {
            let title = format!("{} commented on issue #{}", user.username, issue.number);
            let _ = create_and_broadcast_notification(
                pool,
                broadcaster,
                NotificationRequest {
                    user_id: issue.author_id,
                    actor_id: Some(user.id),
                    repo_id: Some(repository.id),
                    kind: "issue_comment",
                    title: &title,
                    body: Some(&req.body),
                    link: Some(&link),
                    metadata: json!({
                        "repo_owner": owner,
                        "repo_name": repo,
                        "issue_number": issue.number,
                        "subject": issue.title,
                        "comment_id": comment.id,
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
                number: issue.number,
                subject: &issue.title,
                link: &link,
                comment_id: Some(MentionComment::Issue(comment.id)),
                old_text: None,
                text: &req.body,
                notified: &[issue.author_id],
            },
        )
        .await;
    }

    let (issue_for_hook, comment_for_hook) = (issue.clone(), comment.clone());
    let repository_for_hook = repository.clone();
    let (issue_author_id, comment_author_id, sender_id) =
        (issue.author_id, comment.author_id, user.id);
    webhooks::emit(
        pool,
        &repository,
        events::ISSUE_COMMENT,
        vec![issue_author_id, comment_author_id, sender_id],
        move |owner, people| {
            let (issue_author, comment_author, sender) = (
                people.get(issue_author_id),
                people.get(comment_author_id),
                people.get(sender_id),
            );
            payloads::issue_comment(
                "created",
                &comment_for_hook,
                &issue_for_hook,
                &issue_author,
                &comment_author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );
    Ok(comment)
}

/// Changes the body of a comment. Only its author may, and only while the thread is open.
pub async fn edit_issue_comment(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    target: CommentTarget,
    req: CreateCommentRequest,
    expected_body: Option<&str>,
) -> Result<db::models::IssueComment, OperationError> {
    let CommentTarget {
        owner,
        repo,
        number,
        comment_id,
    } = target;
    let user = super::actor(pool, actor_id).await?;
    let (repository, issue) = read_issue(pool, &user, &owner, &repo, number).await?;
    if let Some(refusal) = refuse_change(&repository, &issue) {
        return Err(refusal);
    }
    let previous = issue_comment(pool, &issue, comment_id).await?;
    if expected_body.is_some_and(|body| body != previous.body) {
        return Err(OperationError::Conflict {
            code: "stale_comment",
            message: "The comment changed. Reload before ticking a task.".into(),
        });
    }
    if previous.author_id != user.id {
        return Err(OperationError::Denied {
            code: "comment_forbidden",
            message: "Only the author can edit this comment.".to_owned(),
        });
    }
    if let Err(message) = super::validate_body(&req.body, true) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }
    let comment = db::issues::edit_issue_comment(
        pool,
        user.id,
        repository.id,
        issue.id,
        comment_id,
        &req.body,
        &previous.body,
    )
    .await
    .map_err(super::write_error)?
    .ok_or_else(|| match expected_body {
        Some(_) => OperationError::Conflict {
            code: "conflict",
            message: "This comment changed; reload before ticking a task.".to_owned(),
        },
        None => OperationError::Denied {
            code: "comment_forbidden",
            message: "You cannot edit this comment.".to_owned(),
        },
    })?;

    // Only the names the edit adds notify; the others were notified when first written. The
    // author's notice for the comment is not a mention, so the author is not excluded here.
    if mentions_anyone(&req.body) {
        let base = format!("/{}/{}/issues/{}", owner, repo, issue.number);
        let link = comment_link(
            &base,
            comment.id,
            db::issues::issue_comment_position(pool, comment.id).await,
        );
        notify_mentions(
            pool,
            broadcaster,
            MentionSource {
                actor: &user,
                repository: &repository,
                owner: &owner,
                repo: &repo,
                number: issue.number,
                subject: &issue.title,
                link: &link,
                comment_id: Some(MentionComment::Issue(comment.id)),
                old_text: Some(&previous.body),
                text: &req.body,
                notified: &[],
            },
        )
        .await;
    }
    let (issue_for_hook, comment_for_hook) = (issue.clone(), comment.clone());
    let repository_for_hook = repository.clone();
    let (issue_author_id, comment_author_id, sender_id) =
        (issue.author_id, comment.author_id, user.id);
    webhooks::emit(
        pool,
        &repository,
        events::ISSUE_COMMENT,
        vec![issue_author_id, comment_author_id, sender_id],
        move |owner, people| {
            let (issue_author, comment_author, sender) = (
                people.get(issue_author_id),
                people.get(comment_author_id),
                people.get(sender_id),
            );
            payloads::issue_comment(
                "edited",
                &comment_for_hook,
                &issue_for_hook,
                &issue_author,
                &comment_author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );
    Ok(comment)
}

/// Deletes a comment and the notifications about it. The author may delete their own comment and
/// a repository maintainer may delete anyone's.
pub async fn delete_issue_comment(
    pool: &db::DbPool,
    actor_id: i64,
    target: CommentTarget,
) -> Result<(), OperationError> {
    let CommentTarget {
        owner,
        repo,
        number,
        comment_id,
    } = target;
    let user = super::actor(pool, actor_id).await?;
    let (repository, issue) = read_issue(pool, &user, &owner, &repo, number).await?;
    if let Some(refusal) = refuse_change(&repository, &issue) {
        return Err(refusal);
    }
    let previous = issue_comment(pool, &issue, comment_id).await?;
    let removed =
        db::issues::delete_issue_comment(pool, user.id, repository.id, issue.id, comment_id)
            .await
            .map_err(super::write_error)?;
    if removed {
        let (issue_for_hook, comment_for_hook) = (issue.clone(), previous.clone());
        let repository_for_hook = repository.clone();
        let (issue_author_id, comment_author_id, sender_id) =
            (issue.author_id, previous.author_id, user.id);
        webhooks::emit(
            pool,
            &repository,
            events::ISSUE_COMMENT,
            vec![issue_author_id, comment_author_id, sender_id],
            move |owner, people| {
                let (issue_author, comment_author, sender) = (
                    people.get(issue_author_id),
                    people.get(comment_author_id),
                    people.get(sender_id),
                );
                payloads::issue_comment(
                    "deleted",
                    &comment_for_hook,
                    &issue_for_hook,
                    &issue_author,
                    &comment_author,
                    Scope {
                        repo: &repository_for_hook,
                        owner,
                        sender: &sender,
                    },
                )
            },
        );
        Ok(())
    } else {
        Err(OperationError::Denied {
            code: "comment_forbidden",
            message: "You cannot delete this comment.".to_owned(),
        })
    }
}
