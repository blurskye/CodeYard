use super::{comment_link, OperationError};
use crate::backend::db;
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::{mentions_anyone, notify_mentions, MentionSource};
use crate::backend::notifications::NotificationBroadcaster;
use serde::Deserialize;
use serde_json::json;
#[derive(Deserialize)]
pub struct CreateCommentRequest {
    pub body: String,
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

    let issue = db::issues::get_issue(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Issue not found".to_owned(),
        })?;

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
                old_text: None,
                text: &req.body,
                notified: &[issue.author_id],
            },
        )
        .await;
    }

    Ok(comment)
}
