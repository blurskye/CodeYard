//! What changed on an issue or pull request: state, title, labels, assignees and milestone.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Most events one page loads for an issue or pull request.
pub const EVENT_LIMIT: i64 = 200;

/// One change. `detail` is the label, user, milestone or new title; `previous` is the old title.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TimelineEvent {
    pub actor: Option<String>,
    pub kind: String,
    pub detail: String,
    pub previous: String,
    pub created_at: DateTime<Utc>,
}

/// What the event is about: the `subject_kind` stored with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Issue,
    PullRequest,
}

impl Subject {
    pub fn name(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull_request",
        }
    }
}

native_operation!(crate::backend::db::postgres::timeline::record_event,
    crate::backend::db::sqlite::timeline::record_event,
    #[allow(clippy::too_many_arguments)]
    fn record_event(pool: &DbPool, repo_id: i64, subject: &str, subject_id: i64, actor_id: i64, kind: &str, detail: &str, previous: &str) -> Result<()>);

// The events of a subject from `after` up to `before`, each bound optional, oldest first.
native_operation!(crate::backend::db::postgres::timeline::list_events,
    crate::backend::db::sqlite::timeline::list_events,
    fn list_events(
    pool: &DbPool,
    subject: &str,
    subject_id: i64,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<TimelineEvent>>);

/// Records an event on an issue or pull request named by its `AssignedItem`.
pub async fn record_for(
    pool: &DbPool,
    repo_id: i64,
    item: crate::backend::db::assignees::AssignedItem,
    actor_id: i64,
    kind: &str,
    detail: &str,
) {
    use crate::backend::db::assignees::AssignedItem;
    let (subject, id) = match item {
        AssignedItem::Issue(id) => (Subject::Issue, id),
        AssignedItem::PullRequest(id) => (Subject::PullRequest, id),
    };
    record(pool, repo_id, subject, id, actor_id, kind, detail, "").await;
}

/// Records an event. The change has already been saved, so a failure is logged and dropped.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &DbPool,
    repo_id: i64,
    subject: Subject,
    subject_id: i64,
    actor_id: i64,
    kind: &str,
    detail: &str,
    previous: &str,
) {
    let clip = |text: &str| text.chars().take(255).collect::<String>();
    if let Err(error) = record_event(
        pool,
        repo_id,
        subject.name(),
        subject_id,
        actor_id,
        kind,
        &clip(detail),
        &clip(previous),
    )
    .await
    {
        tracing::warn!(%error, kind, "Timeline event was not saved");
    }
}
