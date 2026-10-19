use super::OperationError;
use crate::backend::db;
use crate::backend::notifications::mentions::{notify_mentions, MentionSource};
use crate::backend::notifications::watchers::{notify_watchers, WatchEvent, WatchedItem};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use serde::Deserialize;
#[derive(Deserialize)]
pub struct CreateIssueRequest {
    pub title: String,
    pub body: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateIssueRequest {
    pub expected_state: String,
    pub expected_updated: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
}

pub async fn create_issue(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    owner: String,
    repo: String,
    req: CreateIssueRequest,
) -> Result<db::models::Issue, OperationError> {
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

    let checked = super::validate_title(&req.title)
        .and_then(|_| super::validate_body(req.body.as_deref().unwrap_or(""), false));
    if let Err(message) = checked {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }

    let body = req.body.as_deref().unwrap_or("");
    let issue = db::issues::create_issue(pool, repository.id, user.id, &req.title, body)
        .await
        .map_err(super::write_error)?;

    let link = format!("/{owner}/{repo}/issues/{}", issue.number);
    let notified = notify_watchers(
        pool,
        broadcaster,
        WatchedItem {
            event: WatchEvent::Issue,
            actor: &user,
            repository: &repository,
            owner: &owner,
            repo: &repo,
            number: Some(issue.number),
            title: &issue.title,
            link: &link,
            notified: &[],
        },
    )
    .await;
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
            comment_id: None,
            old_text: None,
            text: body,
            notified: &notified,
        },
    )
    .await;

    let (author_id, sender_id) = (issue.author_id, user.id);
    let issue_for_hook = issue.clone();
    let repository_for_hook = repository.clone();
    webhooks::emit(
        pool,
        &repository,
        events::ISSUES,
        vec![author_id, sender_id],
        move |owner, people| {
            let (author, sender) = (people.get(author_id), people.get(sender_id));
            payloads::issues(
                "opened",
                &issue_for_hook,
                &author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );
    Ok(issue)
}

pub async fn update_issue(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    req: UpdateIssueRequest,
) -> Result<db::models::Issue, OperationError> {
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

    super::require_write(pool, &user, &repository).await?;

    let existing = db::issues::get_issue(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Issue not found".to_owned(),
        })?;

    let requested_state = match req.state.as_deref() {
        Some("open") | Some("closed") => req.state.as_deref(),
        Some(_) => {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "state must be one of: open, closed".to_owned(),
            })
        }
        None => None,
    };

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
        .is_some_and(|title| super::validate_title(title).is_err())
        || req
            .body
            .as_ref()
            .is_some_and(|body| super::validate_body(body, false).is_err())
        || !matches!(req.expected_state.as_str(), "open" | "closed")
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Invalid title, body, or expected state".to_owned(),
        });
    }
    let issue = db::issues::update_issue(
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
    )
    .await
    .map_err(|_| {
        tracing::error!(issue_id = existing.id, "conditional issue update failed");
        OperationError::Unavailable {
            code: "database_error",
            message: "Unable to save this entry; your submission was not confirmed".to_owned(),
        }
    })?
    .ok_or_else(|| OperationError::Conflict {
        code: "conflict",
        message:
            "This entry or your permission changed; reload and review your preserved submission"
                .to_owned(),
    })?;

    let subject = db::timeline::Subject::Issue;
    if issue.title != existing.title {
        db::timeline::record(
            pool,
            repository.id,
            subject,
            existing.id,
            user.id,
            "renamed",
            &issue.title,
            &existing.title,
        )
        .await;
    }
    if issue.state != existing.state {
        let kind = if issue.state == "closed" {
            "closed"
        } else {
            "reopened"
        };
        db::timeline::record(
            pool,
            repository.id,
            subject,
            existing.id,
            user.id,
            kind,
            "",
            "",
        )
        .await;
    }
    if let Some(body) = req.body.as_deref() {
        let link = format!("/{owner}/{repo}/issues/{number}");
        notify_mentions(
            pool,
            broadcaster,
            MentionSource {
                actor: &user,
                repository: &repository,
                owner: &owner,
                repo: &repo,
                number,
                subject: &issue.title,
                link: &link,
                comment_id: None,
                old_text: Some(&existing.body),
                text: body,
                notified: &[],
            },
        )
        .await;
    }

    let action = match (issue.state == existing.state, issue.state.as_str()) {
        (true, _) => "edited",
        (false, "closed") => "closed",
        (false, _) => "reopened",
    };
    let (author_id, sender_id) = (issue.author_id, user.id);
    let issue_for_hook = issue.clone();
    let repository_for_hook = repository.clone();
    webhooks::emit(
        pool,
        &repository,
        events::ISSUES,
        vec![author_id, sender_id],
        move |owner, people| {
            let (author, sender) = (people.get(author_id), people.get(sender_id));
            payloads::issues(
                action,
                &issue_for_hook,
                &author,
                Scope {
                    repo: &repository_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    );
    Ok(issue)
}
