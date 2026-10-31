use super::{database_error, OperationError};
use crate::backend::db;
use crate::backend::db::users::accounts::AccountCredential;
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::{
    mentions_anyone, notify_mentions, MentionComment, MentionSource,
};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use serde::Deserialize;
use serde_json::json;

#[derive(Default, Deserialize)]
pub struct CreateCommentRequest {
    pub body: String,
    pub repository_id: Option<i64>,
    pub issue_id: Option<i64>,
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
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    super::require_read(pool, user, &repository).await?;
    let issue = db::issues::get_issue(pool, repository.id, number)
        .await
        .map_err(database_error)?
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
        .map_err(database_error)?
        .filter(|comment| comment.issue_id == issue.id)
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Comment not found".to_owned(),
        })
}

async fn require_change(
    pool: &db::DbPool,
    user: &db::models::User,
    repository: &db::models::Repository,
    issue: &db::models::Issue,
) -> Result<(), OperationError> {
    if repository.is_archived {
        return Err(OperationError::Denied {
            code: "repository_archived",
            message: "This repository is archived and read-only.".to_owned(),
        });
    }
    if issue.locked && !crate::backend::repositories::access::can_write(pool, user, repository).await.map_err(database_error)? {
        return Err(OperationError::Denied {
            code: "conversation_locked",
            message: "This conversation is locked.".to_owned(),
        });
    }
    Ok(())
}

pub async fn create_issue_comment(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    credential: &AccountCredential<'_>,
    owner: String,
    repo: String,
    number: i32,
    req: CreateCommentRequest,
) -> Result<db::models::IssueComment, OperationError> {
    let user = super::actor(pool, actor_id).await?;
    let (repository, issue) = read_issue(pool, &user, &owner, &repo, number).await?;
    require_change(pool, &user, &repository, &issue).await?;

    if let Err(message) = super::validate_body(&req.body, true) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }

    if req.repository_id.is_some_and(|id| id != repository.id)
        || req.issue_id.is_some_and(|id| id != issue.id)
    {
        return Err(OperationError::Conflict {
            code: "stale_target",
            message: "The comment target changed. Reload before submitting.".into(),
        });
    }
    let comment =
        db::issues::add_issue_comment(pool, repository.id, issue.id, user.id, &req.body, credential)
            .await
            .map_err(super::write_error)?;

    let base = format!("/{}/{}/issues/{}", owner, repo, issue.number);
    let link = format!("{base}?comment={}#comment-{}", comment.id, comment.id);
    let title = format!("{} commented on issue #{}", user.username, issue.number);
    let notice = NotificationRequest {
        user_id: issue.author_id,
        actor_id: Some(user.id),
        repo_id: Some(repository.id),
        kind: "issue_comment",
        title: &title,
        body: Some(&req.body),
        link: Some(&link),
        metadata: json!({"repo_owner":owner,"repo_name":repo,"issue_number":issue.number,"subject":issue.title,"comment_id":comment.id}),
    };
    let mut notified = Vec::new();
    if issue.author_id != user.id
        && create_and_broadcast_notification(pool, broadcaster, notice.clone())
            .await
            .is_ok()
    {
        notified.push(issue.author_id);
    }
    // A mention comes before the subscriber notice, so a mentioned subscriber hears it as one.
    let mentioned = notify_mentions(
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
            notified: &notified,
        },
    )
    .await;
    notified.extend(mentioned);
    crate::backend::notifications::create::notify_subscribers(pool, broadcaster, notice, &notified)
        .await;

    // The comment moved the issue's revision, so the hook names the issue as it is now.
    let commented = match db::issues::get_issue_by_id(pool, issue.id).await {
        Ok(Some(current)) if current.repo_id == repository.id => current,
        Ok(_) => issue,
        Err(error) => {
            tracing::warn!(%error, "Commented issue was not read again for its webhook");
            issue
        }
    };
    emit_comment_webhook(pool, &repository, &commented, &comment, user.id, ("created", None))
        .await;
    Ok(comment)
}

/// Sends the issue_comment webhook for a comment change that has committed.
async fn emit_comment_webhook(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    issue: &db::models::Issue,
    comment: &db::models::IssueComment,
    sender_id: i64,
    (action, previous_body): (&'static str, Option<String>),
) {
    let (issue, comment, repository_for_hook) = (issue.clone(), comment.clone(), repository.clone());
    let (issue_author_id, comment_author_id) = (issue.author_id, comment.author_id);
    webhooks::emit(
        pool,
        repository,
        events::ISSUE_COMMENT,
        vec![issue_author_id, comment_author_id, sender_id],
        move |owner, people| {
            let (issue_author, comment_author, sender) = (
                people.get(issue_author_id),
                people.get(comment_author_id),
                people.get(sender_id),
            );
            payloads::issue_comment(
                action,
                previous_body.as_deref(),
                &comment,
                &issue,
                &issue_author,
                &comment_author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    )
    .await;
}

/// The author or a repository writer may edit; locked threads remain writable by writers.
pub async fn edit_issue_comment(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    credential: &AccountCredential<'_>,
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
    require_change(pool, &user, &repository, &issue).await?;
    let previous = issue_comment(pool, &issue, comment_id).await?;
    if previous.author_id != user.id && !crate::backend::repositories::access::can_write(pool, &user, &repository).await.map_err(database_error)? {
        return Err(OperationError::Denied {
            code: "comment_forbidden",
            message: "Only the author or a repository writer can edit this comment.".to_owned(),
        });
    }
    if expected_body.is_some_and(|body| body != previous.body) {
        return Err(OperationError::Conflict {
            code: "stale_comment",
            message: "The comment changed. Your draft is preserved; reload before saving.".into(),
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
        credential,
    )
    .await
    .map_err(super::write_error)?
    .ok_or_else(|| match expected_body {
        Some(_) => OperationError::Conflict {
            code: "conflict",
            message: "This comment changed; your draft is preserved. Reload before saving."
                .to_owned(),
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
        let link = format!("{base}?comment={}#comment-{}", comment.id, comment.id);
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
    emit_comment_webhook(
        pool,
        &repository,
        &issue,
        &comment,
        user.id,
        ("edited", Some(previous.body)),
    )
    .await;
    Ok(comment)
}

/// Deletes a comment and the notifications about it. The author may delete their own comment and
/// a repository writer may delete anyone's.
pub async fn delete_issue_comment(
    pool: &db::DbPool,
    actor_id: i64,
    credential: &AccountCredential<'_>,
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
    require_change(pool, &user, &repository, &issue).await?;
    issue_comment(pool, &issue, comment_id).await?;
    let removed = db::issues::delete_issue_comment(
        pool,
        user.id,
        repository.id,
        issue.id,
        comment_id,
        credential,
    )
    .await
    .map_err(super::write_error)?;
    // The hook carries the comment as the delete removed it, not as it was read before.
    let Some(removed) = removed else {
        return Err(OperationError::Denied {
            code: "comment_forbidden",
            message: "You cannot delete this comment.".to_owned(),
        });
    };
    emit_comment_webhook(pool, &repository, &issue, &removed, user.id, ("deleted", None)).await;
    Ok(())
}
