//! Stable per-recipient repository subjects, projected and acknowledged through the native inbox.
use crate::backend::db::models::{Notification, Repository};
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::FromRow;

native_operation!(crate::backend::db::postgres::notifications::records::threads::subscribers,
    crate::backend::db::sqlite::notifications::records::threads::subscribers,
    fn subscribers(pool:&DbPool,repo_id:i64,kind:&str,key:&str)->Result<Vec<i64>>);

pub(crate) fn allows_delivery(subscribed: bool, ignored: bool, kind: &str) -> bool {
    !ignored
        && (subscribed
            || matches!(
                kind,
                "mention"
                    | "assigned"
                    | "pull_request_review_requested"
                    | "issue_assigned"
                    | "pull_request_assigned"
                    | "pull_request_comment_reply"
                    | "ci_completed"
            ))
}

#[derive(Clone, Debug, FromRow)]
pub struct NotificationThread {
    pub id: i64,
    pub user_id: i64,
    pub scope_repository_id: i64,
    pub subject_type: String,
    pub subject_key: String,
    pub created_at: DateTime<Utc>,
}
#[derive(Clone, Debug, FromRow)]
pub(crate) struct ThreadSummary {
    pub thread_id: i64,
    pub scope_repository_id: i64,
    pub latest_id: i64,
    pub updated_at: DateTime<Utc>,
    pub last_read_at: Option<DateTime<Utc>>,
    pub unread: bool,
}
#[derive(Clone)]
pub(crate) enum ThreadTarget {
    Pull {
        number: i32,
        title: String,
        author: i64,
    },
    Issue {
        number: i32,
        title: String,
        author: i64,
    },
    Release {
        id: i64,
        title: String,
        author: i64,
    },
    Other,
}
#[derive(Clone, Debug, Default)]
pub struct ThreadFilter {
    pub repository_id: Option<i64>,
    pub all: bool,
    pub participating: bool,
    pub since: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub limit: i64,
    pub offset: i64,
}
#[derive(Clone)]
pub enum ThreadAction {
    List(ThreadFilter),
    Get(i64),
    Read(i64),
    Done(i64),
    ReadAll {
        repository_id: Option<i64>,
        last_read_at: Option<DateTime<Utc>>,
        read: bool,
    },
    GetSubscription(i64),
    SetSubscription {
        thread_id: i64,
        subscribed: bool,
        ignored: bool,
    },
    DeleteSubscription(i64),
}
impl ThreadAction {
    pub(crate) fn requires_write(&self) -> bool {
        !matches!(
            self,
            Self::List(_) | Self::Get(_) | Self::GetSubscription(_)
        )
    }
}
pub struct ThreadView {
    pub thread: NotificationThread,
    pub notification: Notification,
    pub repository: Repository,
    pub owner: String,
    pub updated_at: DateTime<Utc>,
    pub last_read_at: Option<DateTime<Utc>>,
    pub unread: bool,
    pub title: String,
    pub subject_type: String,
    pub subject_url: String,
    pub latest_comment_url: String,
    pub reason: String,
}
impl ThreadView {
    pub(crate) fn new(
        thread: NotificationThread,
        mut notification: Notification,
        repository: Repository,
        owner: String,
        summary: ThreadSummary,
        target: ThreadTarget,
        comment: Option<(i64, bool)>,
    ) -> Self {
        use crate::backend::api::github::repo_url;
        super::records::refresh_link(&mut notification, &owner, &repository.name);
        let repo = repo_url(&owner, &repository.name);
        let (kind, title, url, author) = match target {
            ThreadTarget::Pull {
                number,
                title,
                author,
            } => (
                "PullRequest",
                title,
                format!("{repo}/pulls/{number}"),
                Some(author),
            ),
            ThreadTarget::Issue {
                number,
                title,
                author,
            } => (
                "Issue",
                title,
                format!("{repo}/issues/{number}"),
                Some(author),
            ),
            ThreadTarget::Release { id, title, author } => (
                "Release",
                title,
                format!("{repo}/releases/{id}"),
                Some(author),
            ),
            ThreadTarget::Other => {
                let title = notification.metadata["subject"]
                    .as_str()
                    .unwrap_or(&notification.title)
                    .to_owned();
                let (kind, url) = match thread.subject_type.as_str() {
                    "Repository" => ("Repository", repo.clone()),
                    "Commit" => ("Commit", format!("{repo}/commits/{}", thread.subject_key)),
                    "CheckSuite" => (
                        "CheckSuite",
                        notification
                            .link
                            .as_deref()
                            .filter(|l| l.starts_with('/'))
                            .map(|l| {
                                format!("{}{l}", crate::backend::api::http::config::link_origin())
                            })
                            .unwrap_or_default(),
                    ),
                    _ => ("Discussion", String::new()),
                };
                (kind, title, url, None)
            }
        };
        let latest_comment_url = if let Some(id) = notification.metadata["comment_id"]
            .as_i64()
            .filter(|_| kind == "Issue")
        {
            let id = crate::backend::webhooks::payloads::conversation_comment_id(id, false);
            format!("{repo}/issues/comments/{id}")
        } else if let Some((id, inline)) = comment.filter(|_| kind == "PullRequest") {
            if inline {
                format!("{repo}/pulls/comments/{id}")
            } else {
                let id = crate::backend::webhooks::payloads::conversation_comment_id(id, true);
                format!("{repo}/issues/comments/{id}")
            }
        } else if let Some(id) = notification.metadata["commit_comment_id"]
            .as_i64()
            .filter(|_| kind == "Commit")
        {
            format!("{repo}/comments/{id}")
        } else if let Some(id) = notification.metadata["review_id"].as_i64() {
            if kind == "PullRequest" {
                format!("{url}/reviews/{id}")
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let reason = match notification.kind.as_str() {
            "mention" => "mention",
            "ci_completed" => "ci_activity",
            "pull_request_review_requested" => "review_requested",
            "assigned" | "issue_assigned" | "pull_request_assigned" => "assign",
            _ if author == Some(thread.user_id) => "author",
            _ => "subscribed",
        }
        .to_owned();
        Self {
            thread,
            notification,
            repository,
            owner,
            updated_at: summary.updated_at,
            last_read_at: summary.last_read_at,
            unread: summary.unread,
            title,
            subject_type: kind.into(),
            subject_url: url,
            latest_comment_url,
            reason,
        }
    }
}
#[derive(FromRow)]
pub(crate) struct ThreadProjection {
    pub thread_id: i64,
    pub owner: Option<String>,
    pub pull_number: Option<i32>,
    pub issue_number: Option<i32>,
    pub release_id: Option<i64>,
    pub title: Option<String>,
    pub author: Option<i64>,
    pub comment_id: Option<i64>,
    pub inline: Option<bool>,
}

pub(crate) fn hydrate(
    summaries: Vec<ThreadSummary>,
    threads: Vec<NotificationThread>,
    notifications: Vec<Notification>,
    repositories: Vec<Repository>,
    projections: Vec<ThreadProjection>,
) -> Vec<ThreadView> {
    use std::collections::HashMap;
    let mut threads: HashMap<_, _> = threads.into_iter().map(|t| (t.id, t)).collect();
    let mut notifications: HashMap<_, _> = notifications.into_iter().map(|n| (n.id, n)).collect();
    let repositories: HashMap<_, _> = repositories.into_iter().map(|r| (r.id, r)).collect();
    let mut projections: HashMap<_, _> = projections.into_iter().map(|p| (p.thread_id, p)).collect();
    summaries.into_iter().filter_map(|summary| {
        let thread = threads.remove(&summary.thread_id)?;
        let notification = notifications.remove(&summary.latest_id)?;
        let repository = repositories.get(&summary.scope_repository_id)?.clone();
        let p = projections.remove(&thread.id)?;
        let target = match (p.title, p.author) {
            (Some(title), Some(author)) => {
                if let Some(number) = p.pull_number { ThreadTarget::Pull { number, title, author } }
                else if let Some(number) = p.issue_number { ThreadTarget::Issue { number, title, author } }
                else if let Some(id) = p.release_id { ThreadTarget::Release { id, title, author } }
                else { ThreadTarget::Other }
            }
            _ => ThreadTarget::Other,
        };
        Some(ThreadView::new(thread, notification, repository, p.owner?, summary, target, p.comment_id.zip(p.inline)))
    }).collect()
}

#[derive(Clone, Debug, FromRow)]
pub struct ThreadSubscription {
    pub thread_id: i64,
    pub subscribed: bool,
    pub ignored: bool,
    pub created_at: DateTime<Utc>,
}
#[derive(Default)]
pub struct ThreadInbox {
    pub thread: Option<NotificationThread>,
    pub threads: Vec<ThreadView>,
    pub subscription: Option<ThreadSubscription>,
    pub found: bool,
    pub updated: u64,
}

/// Account-only notices have no GitHub repository subject and remain in the web inbox.
pub(crate) fn subject(
    input: &super::records::NewNotification<'_>,
) -> Option<(&'static str, String)> {
    let repo = input.repo_id.filter(|id| *id > 0)?;
    if let Some(id) = input.metadata["run_id"]
        .as_str()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
    {
        return Some(("CheckSuite", id.to_string()));
    }
    if let Some(tag) = input.metadata["release_tag"]
        .as_str()
        .or_else(|| {
            (input.kind == "release_published")
                .then(|| input.metadata["subject"].as_str())
                .flatten()
        })
        .filter(|tag| !tag.is_empty())
    {
        return Some(("Release", tag.into()));
    }
    // Native issue/pull numbers share a repository sequence. Keeping this identity also
    // preserves the thread when an issue is promoted to a pull request.
    for field in ["pull_number", "issue_number", "number"] {
        if let Some(number) = input.metadata[field]
            .as_i64()
            .or_else(|| input.metadata[field].as_str()?.parse().ok())
            .filter(|id| *id > 0 && *id <= i32::MAX as i64)
        {
            return Some(("Discussion", number.to_string()));
        }
    }
    // Comments on different commits are different conversations.
    if let Some(sha) = input.metadata["commit_sha"]
        .as_str()
        .filter(|sha| crate::backend::git::is_full_commit_id(sha))
    {
        return Some(("Commit", sha.to_ascii_lowercase()));
    }
    Some(("Repository", repo.to_string()))
}
