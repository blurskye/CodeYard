//! What changed on an issue or pull request: state, title, labels, assignees and milestone.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Most events one page loads for an issue or pull request.
pub const EVENT_LIMIT: i64 = 200;

/// One change. `detail` is the label, user, milestone or new title; `previous` is the old
/// title for a rename or the retained label color for a label change.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TimelineEvent {
    pub actor: Option<String>,
    pub kind: String,
    pub detail: String,
    pub previous: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub repo_id: i64,
    pub subject_kind: String,
    pub subject_id: i64,
    pub actor_id: Option<i64>,
    pub kind: String,
    pub detail: String,
    pub previous: String,
    pub created_at: DateTime<Utc>,
}

native_operation!(crate::backend::db::postgres::timeline::get_event,
    crate::backend::db::sqlite::timeline::get_event,
    fn get_event(pool: &DbPool, repo_id: i64, id: i64) -> Result<Option<Event>>);

native_operation!(crate::backend::db::postgres::timeline::list_entries,
    crate::backend::db::sqlite::timeline::list_entries,
    fn list_entries(pool: &DbPool, repo_id: i64, item: Option<crate::backend::db::assignees::AssignedItem>, timeline: bool, descending: bool, limit: i64, offset: i64) -> Result<Vec<(String, i64)>>);

pub(crate) fn entries_query(descending: bool) -> String {
    let direction = if descending { "DESC" } else { "ASC" };
    format!(
        r"WITH entries AS (
        SELECT 'event' AS kind,e.id,e.created_at FROM timeline_events e WHERE e.repo_id=$1
            AND e.subject_kind=COALESCE($2,e.subject_kind) AND (COALESCE($3,e.subject_id)=e.subject_id)
            AND ((e.subject_kind='issue' AND EXISTS (SELECT 1 FROM issues i WHERE i.id=e.subject_id AND i.repo_id=e.repo_id))
                OR (e.subject_kind='pull_request' AND EXISTS (SELECT 1 FROM pull_requests p WHERE p.id=e.subject_id AND p.repo_id=e.repo_id)))
        UNION ALL SELECT 'issue_comment',c.id,c.created_at FROM issue_comments c JOIN issues i ON i.id=c.issue_id
            WHERE $4 AND i.repo_id=$1 AND ($2 IS NULL OR $2='issue' AND i.id=$3)
        UNION ALL SELECT 'pr_comment',c.id,c.created_at FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id
            WHERE $4 AND p.repo_id=$1 AND c.path IS NULL AND c.review_id IS NULL AND ($2 IS NULL OR $2='pull_request' AND p.id=$3)
        UNION ALL SELECT 'review',v.id,v.created_at FROM pr_reviews v JOIN pull_requests p ON p.id=v.pull_request_id
            WHERE $4 AND p.repo_id=$1 AND v.state<>'pending' AND ($2 IS NULL OR $2='pull_request' AND p.id=$3)
        ) SELECT kind,id FROM entries ORDER BY created_at {direction},id {direction},kind LIMIT $5 OFFSET $6"
    )
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
    offset: i64,
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
