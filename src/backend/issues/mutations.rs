use super::{database_error, OperationError};
use crate::backend::db;
use crate::backend::notifications::mentions::{notify_mentions, MentionSource};
use crate::backend::notifications::watchers::{notify_watchers, WatchEvent, WatchedItem};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use serde::Deserialize;
use std::sync::Arc;
#[derive(Deserialize)]
pub struct CreateIssueRequest {
    pub title: String,
    pub body: Option<String>,
    #[serde(default)]
    pub repository_id: Option<i64>,
    #[serde(default)]
    pub parent_issue_id: Option<i64>,
    #[serde(skip)]
    pub token: Option<i64>,
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

async fn emit_issue_event(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    issue: &db::models::Issue,
    sender_id: i64,
    action: &'static str,
    previous: Option<&db::models::Issue>,
) {
    let author_id = issue.author_id;
    let mut changes = serde_json::Map::new();
    if let Some(previous) = previous {
        if previous.title != issue.title {
            changes.insert("title".into(), serde_json::json!({"from": previous.title}));
        }
        if previous.body != issue.body {
            changes.insert("body".into(), serde_json::json!({"from": previous.body}));
        }
    }
    let issue = issue.clone();
    let document = repository.clone();
    webhooks::emit(pool, repository, events::ISSUES, vec![author_id, sender_id], move |owner, people| {
        let (author, sender) = (people.get(author_id), people.get(sender_id));
        let mut payload = payloads::issues(action, &issue, &author, Scope { repo: &document, owner, sender: &sender });
        if action == "edited" { payload["changes"] = serde_json::Value::Object(changes.clone()); }
        payload
    }).await;
}

/// The duplicate relationship, closure and timeline event commit together; the closure's
/// notifications finish even if the initiating HTTP request is cancelled.
pub async fn close_duplicate(
    pool: &db::DbPool,
    broadcaster: &Arc<NotificationBroadcaster>,
    actor: impl Into<db::users::accounts::AccountActor>,
    repo: i64,
    issue: i64,
    duplicate: i64,
) -> Result<db::models::Issue, OperationError> {
    let (pool, broadcaster, actor) = (pool.clone(), broadcaster.clone(), actor.into());
    tokio::spawn(async move {
        let user = super::actor(&pool, actor.user_id()).await?;
        let (repository, owner) = super::repository(&pool, repo).await?;
        let (previous, saved) = db::issues::management::close_duplicate_with_previous(&pool, actor, repo, issue, duplicate)
            .await
            .map_err(|error| {
                use db::issues::management::Refusal;
                match error.downcast_ref::<Refusal>() {
                    Some(Refusal::Missing) => OperationError::Missing { code: "not_found", message: error.to_string() },
                    Some(Refusal::Denied) => OperationError::Denied { code: "forbidden", message: error.to_string() },
                    Some(_) => OperationError::Invalid { code: "validation_error", message: error.to_string() },
                    None => database_error(error),
                }
            })?;
        if previous.state != saved.state {
            after_update(&pool, &broadcaster, &repository, &user, &owner, &previous, &saved).await;
        }
        Ok(saved)
    })
    .await
    .unwrap_or_else(|_| Err(OperationError::Unavailable {
        code: "publication_interrupted",
        message: "Duplicate closure was interrupted; check the issue before retrying.".into(),
    }))
}

pub async fn create_issue(
    pool: &db::DbPool,
    broadcaster: &Arc<NotificationBroadcaster>,
    actor: impl Into<db::users::accounts::AccountActor>,
    owner: String,
    repo: String,
    req: CreateIssueRequest,
) -> Result<db::models::Issue, OperationError> {
    create_issue_with_metadata(pool, broadcaster, actor, owner, repo, req, db::issues::OwnedIssueMetadata::default()).await
}

pub async fn create_issue_with_metadata(
    pool: &db::DbPool,
    broadcaster: &Arc<NotificationBroadcaster>,
    actor: impl Into<db::users::accounts::AccountActor>,
    owner: String,
    repo: String,
    req: CreateIssueRequest,
    metadata: db::issues::OwnedIssueMetadata,
) -> Result<db::models::Issue, OperationError> {
    let (pool, broadcaster, actor) = (pool.clone(), broadcaster.clone(), actor.into());
    tokio::spawn(async move {
        create_issue_owned(&pool, &broadcaster, actor, owner, repo, req, metadata.borrowed()).await
    }).await.unwrap_or_else(|_| Err(OperationError::Unavailable {
        code: "publication_interrupted",
        message: "Issue creation was interrupted; check the repository before retrying.".into(),
    }))
}

async fn create_issue_owned(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: db::users::accounts::AccountActor,
    owner: String,
    repo: String,
    req: CreateIssueRequest,
    mut metadata: db::issues::IssueMetadata<'_>,
) -> Result<db::models::Issue, OperationError> {
    let user = super::actor(pool, actor.user_id()).await?;

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
    metadata.parent = req.parent_issue_id.or(metadata.parent);
    let issue = db::issues::create_issue_with_metadata(pool, repository.id, actor, &req.title, body, metadata)
        .await.map_err(super::write_error)?;

    // Persistence has committed. Assignment effects are best-effort and cannot turn a successful
    // create into a metadata error that invites the caller to create the same issue again.
    if !metadata.assignees.is_empty() {
        match db::users::accounts::get_users_by_ids(pool, metadata.assignees).await {
            Ok(users) => {
                let users: Vec<_> = users.iter().collect();
                super::assignments::announce(pool, broadcaster, &super::assignments::Target {
                    actor: &user, repository: &repository, owner: &owner,
                    item: db::assignees::AssignedItem::Issue(issue.id), number: issue.number, title: &issue.title,
                }, &users).await;
            }
            Err(error) => tracing::warn!(issue_id = issue.id, ?error, "Initial issue assignments could not be announced"),
        }
    }
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

    emit_issue_event(pool, &repository, &issue, user.id, "opened", None).await;
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

    if req.state_reason.as_deref().is_some_and(|reason| {
        !matches!(
            reason,
            "completed" | "not_planned" | "reopened" | "duplicate"
        )
    }) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Invalid state reason".into(),
        });
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

    after_update(pool, broadcaster, &repository, &user, &owner, &existing, &issue).await;
    Ok(issue)
}

async fn after_update(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    repository: &db::models::Repository,
    user: &db::models::User,
    owner: &str,
    existing: &db::models::Issue,
    issue: &db::models::Issue,
) {
    let repo = &repository.name;
    let number = issue.number;
    if let Some(body) = (issue.body != existing.body).then_some(issue.body.as_str()) {
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
    emit_issue_event(pool, &repository, &issue, user.id, action, Some(&existing)).await;
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
