use super::{database_error, OperationError};
use crate::backend::db;
use crate::backend::notifications::create::{
    create_and_broadcast_notification, notify_subscribers, NotificationRequest,
};
use crate::backend::notifications::mentions::{notify_mentions, MentionSource};
use crate::backend::notifications::watchers::{notify_watchers, WatchEvent, WatchedItem};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use serde::Deserialize;
#[derive(Deserialize)]
pub struct CreateIssueRequest {
    pub title: String,
    pub body: Option<String>,
    #[serde(default)]
    pub repository_id: Option<i64>,
    #[serde(default)]
    pub parent_issue_id: Option<i64>,
    #[serde(skip)]
    pub parent_repository_id: Option<i64>,
}

#[derive(Clone, Deserialize)]
pub struct UpdateIssueRequest {
    pub expected_state: String,
    pub expected_updated: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    pub state_reason: Option<String>,
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
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    super::require_read(pool, &user, &repository).await?;
    if req.repository_id.is_some_and(|id| id != repository.id) {
        return Err(OperationError::Conflict {
            code: "conflict",
            message: "The repository changed. Review your draft before submitting it again."
                .to_owned(),
        });
    }

    let checked = super::validate_title(&req.title)
        .and_then(|_| super::validate_body(req.body.as_deref().unwrap_or(""), false));
    if let Err(message) = checked {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }

    let body = req.body.as_deref().unwrap_or("");
    let issue = db::issues::create_issue_with_metadata(
        pool, repository.id, user.id, &req.title, body,
        db::issues::IssueMetadata {
            parent: req.parent_issue_id,
            parent_repository: req.parent_repository_id,
            ..Default::default()
        },
    ).await.map_err(super::write_error)?;

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
    )
    .await;
    Ok(issue)
}

pub async fn update_issue(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    repository_id: i64,
    number: i32,
    req: UpdateIssueRequest,
) -> Result<db::models::Issue, OperationError> {
    let user = super::actor(pool, actor_id).await?;
    let (repository, owner) = super::repository(pool, repository_id).await?;
    let repo = repository.name.clone();

    super::require_read(pool, &user, &repository).await?;

    let existing = db::issues::get_issue(pool, repository.id, number)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Issue not found".to_owned(),
        })?;
    // An issue's author edits and closes it with read access, as on GitHub.
    if existing.author_id != user.id {
        super::require_write(pool, &user, &repository).await?;
    }

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
    if !db::issues::valid_state_reason(requested_state.unwrap_or(&existing.state), req.state_reason.as_deref()) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Invalid state reason".into(),
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
    let issue = db::issues::update_issue_with_reason(
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
        req.state_reason.as_deref(),
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
    // The update matched the expected state, so that was the state it changed from.
    let state_changed = issue.state != req.expected_state;
    if state_changed {
        notify_state_change(pool, broadcaster, &user, (&repository, &owner), &issue).await;
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

    let (author_id, sender_id) = (issue.author_id, user.id);
    // Text edits and state transitions are independent effects of one committed update.
    let mut changes = serde_json::Map::new();
    if issue.title != existing.title {
        changes.insert("title".into(), serde_json::json!({"from": existing.title}));
    }
    if issue.body != existing.body {
        changes.insert("body".into(), serde_json::json!({"from": existing.body}));
    }
    let mut actions = Vec::with_capacity(2);
    if !changes.is_empty() {
        actions.push("edited");
    }
    if state_changed {
        actions.push(if issue.state == "closed" { "closed" } else { "reopened" });
    }
    for action in actions {
        let issue_for_hook = issue.clone();
        let repository_for_hook = repository.clone();
        let changes = changes.clone();
        webhooks::emit(
            pool,
            &repository,
            events::ISSUES,
            vec![author_id, sender_id],
            move |owner, people| {
                let (author, sender) = (people.get(author_id), people.get(sender_id));
                let mut payload = payloads::issues(
                    action,
                    &issue_for_hook,
                    &author,
                    Scope {
                        repo: &repository_for_hook,
                        owner,
                        sender: &sender,
                    },
                );
                if action == "edited" {
                    payload["changes"] = serde_json::Value::Object(changes.clone());
                }
                payload
            },
        )
        .await;
    }
    Ok(issue)
}

/// Tells the issue's author and the people subscribed to it that the issue was closed or
/// reopened. The actor is not told about their own change.
pub(super) async fn notify_state_change(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &db::models::User,
    (repository, owner): (&db::models::Repository, &str),
    issue: &db::models::Issue,
) {
    let (kind, verb) = if issue.state == "closed" {
        ("issue_closed", "closed")
    } else {
        ("issue_reopened", "reopened")
    };
    let title = format!("{} {verb} issue #{}", actor.username, issue.number);
    let link = format!("/{owner}/{}/issues/{}", repository.name, issue.number);
    let notice = NotificationRequest {
        user_id: issue.author_id,
        actor_id: Some(actor.id),
        repo_id: Some(repository.id),
        kind,
        title: &title,
        body: Some(&issue.title),
        link: Some(&link),
        metadata: serde_json::json!({"repo_owner":owner,"repo_name":repository.name,"issue_number":issue.number,"subject":issue.title}),
    };
    let mut notified = Vec::new();
    if issue.author_id != actor.id
        && create_and_broadcast_notification(pool, broadcaster, notice.clone())
            .await
            .is_ok()
    {
        notified.push(issue.author_id);
    }
    notify_subscribers(pool, broadcaster, notice, &notified).await;
}

/// Saves an edit from the GitHub API, which carries no form revision: the caller passes the
/// state and time it just read. A concurrent change, such as gh adding labels while it saves the
/// title, moves that time, so read the issue again and retry instead of refusing.
pub async fn update_issue_latest(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    repository_id: i64,
    number: i32,
    mut req: UpdateIssueRequest,
) -> Result<db::models::Issue, OperationError> {
    for _ in 0..3 {
        let refused = match update_issue(
            pool,
            broadcaster,
            actor_id,
            repository_id,
            number,
            req.clone(),
        )
        .await
        {
            Err(error @ OperationError::Conflict { .. }) => error,
            result => return result,
        };
        let latest = db::issues::get_issue(pool, repository_id, number)
            .await
            .map_err(database_error)?;
        match latest {
            Some(latest)
                if latest.state != req.expected_state
                    || latest.updated_at.to_rfc3339() != req.expected_updated =>
            {
                req.expected_state = latest.state;
                req.expected_updated = latest.updated_at.to_rfc3339();
            }
            _ => return Err(refused),
        }
    }
    update_issue(pool, broadcaster, actor_id, repository_id, number, req).await
}
