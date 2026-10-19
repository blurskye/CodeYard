use crate::backend::db::models::Notification;
use crate::backend::db::DbPool;
use anyhow::Result;
use serde_json::Value;

pub struct NewNotification<'a> {
    pub user_id: i64,
    pub actor_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub kind: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub link: Option<&'a str>,
    pub metadata: &'a Value,
}

#[derive(Debug, thiserror::Error)]
#[error("Notification is no longer needed")]
pub struct NotificationSuppressed;

native_operation!(crate::backend::db::postgres::notifications::records::create_notification,
    crate::backend::db::sqlite::notifications::records::create_notification,
    fn create_notification(
    pool: &DbPool,
    input: NewNotification<'_>,
) -> Result<Notification>);

/// Current original-session delivery projection. Broadcasts are only hints;
/// payload/count are reloaded under the same native recipient/scope authority.
/// None means session invalid/revoked/restricted; errors mean unavailable.
/// No native lock survives this operation into WebSocket/network IO.
pub struct NotificationDelivery {
    pub notification: Option<Notification>,
    pub unread_count: i64,
}

native_operation!(crate::backend::db::postgres::notifications::records::notification_delivery,
    crate::backend::db::sqlite::notifications::records::notification_delivery,
    fn notification_delivery(pool: &DbPool, session_id: &str, user_id: i64, notification_id: Option<i64>) -> Result<Option<NotificationDelivery>>);

/// An exclusive boundary in persisted (created_at,id) order.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NotificationCursor {
    pub created_at: String,
    pub id: i64,
}

/// Read decisions for one transaction. Each repository is decided once: PostgreSQL decides it
/// when a query first reaches it, under row locks, and SQLite decides every scope of the
/// recipient in its first statement.
pub(crate) struct NotificationScopes {
    pub high_water: i64,
    pub readable: std::collections::BTreeMap<i64, bool>,
}

impl NotificationScopes {
    /// Repositories already decided unreadable. Candidate queries skip them, so hidden
    /// history is never read again batch by batch.
    pub(crate) fn unreadable(&self) -> Vec<i64> {
        self.readable
            .iter()
            .filter(|(_, readable)| !**readable)
            .map(|(scope, _)| *scope)
            .collect()
    }
}

/// The tabs of the notification list. Inbox holds every notification not marked done;
/// Saved holds the saved ones that are not done; Done holds the ones marked done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotificationView {
    #[default]
    Inbox,
    Saved,
    Done,
}

impl NotificationView {
    /// The name used in URLs, forms and the page model.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Saved => "saved",
            Self::Done => "done",
        }
    }
}

/// Which notifications a list or a bulk action covers: one tab, narrowed by the filters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotificationFilter {
    pub view: NotificationView,
    /// The repository a notification is about; None keeps every repository.
    pub repository_id: Option<i64>,
    /// Stored kinds to keep; an empty list keeps every kind.
    pub kinds: Vec<String>,
    /// Plain text matched, ignoring case, against the title and the repository name.
    pub query: Option<String>,
    pub unread_only: bool,
}

impl NotificationFilter {
    pub fn inbox(unread_only: bool) -> Self {
        Self {
            unread_only,
            ..Self::default()
        }
    }

    /// The search text as the queries match it: trimmed and lowercase, or None when empty.
    pub(crate) fn search_text(&self) -> Option<String> {
        self.query
            .as_deref()
            .map(|query| query.trim().to_lowercase())
            .filter(|query| !query.is_empty())
    }
}

/// How many notifications each tab holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ViewCounts {
    pub inbox: i64,
    pub saved: i64,
    pub done: i64,
}

/// The two marks a notification can carry besides read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationMark {
    Saved,
    Done,
}

impl NotificationMark {
    /// The column that stores the mark's time; NULL means the mark is not set.
    pub(crate) fn column(self) -> &'static str {
        match self {
            Self::Saved => "saved_at",
            Self::Done => "done_at",
        }
    }
}

/// The tab counts and the repositories the repository filter can name, over the
/// notifications the recipient can read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotificationCounts {
    pub views: ViewCounts,
    /// Repositories the recipient can read that have notifications, as (ID, owner/name). The
    /// names are read in the transaction that decided the repositories are readable.
    pub repositories: Vec<(i64, String)>,
}

/// Original-session persisted projection/acknowledgment. None means the original
/// session is invalid, revoked, expired or its recipient is restricted. Valid
/// actions may return no rows; SQL errors propagate. All result fields share one
/// native transaction, including final expiry verification before commit.
pub enum NotificationInboxAction {
    List {
        filter: NotificationFilter,
        limit: i64,
        before: Option<NotificationCursor>,
    },
    Read {
        notification_id: i64,
    },
    ReadAll,
    Save {
        notification_id: i64,
        saved: bool,
    },
    Done {
        notification_id: i64,
        done: bool,
    },
    /// Marks every notification of the filter as done, not only the ones on one page.
    DoneAll {
        filter: NotificationFilter,
    },
}

pub struct NotificationInbox {
    pub notifications: Vec<Notification>,
    pub next_cursor: Option<NotificationCursor>,
    /// Unread notifications in the inbox: the count the unread badge shows.
    pub unread_count: i64,
    /// Filled in by the List action only, in the same transaction as the list.
    pub counts: Option<NotificationCounts>,
    /// Rows acknowledged or changed by this action; repeated Read acknowledges zero.
    pub updated: u64,
}

native_operation!(crate::backend::db::postgres::notifications::records::notification_inbox,
    crate::backend::db::sqlite::notifications::records::notification_inbox,
    fn notification_inbox(pool: &DbPool, session_id: &str, user_id: i64, action: NotificationInboxAction) -> Result<Option<NotificationInbox>>);

// Persisted projection/ack contract: user_id is the original authenticated
// recipient, not a replacement administrator. Missing, banned or password-change
// restricted recipients get empty/zero/None (including account notifications).
// Repository notifications require current canonical Read, including personal-
// only platform-admin access; hidden/foreign history is never acknowledged.
// Immutable scope_repository_id owns provenance, not the nullable repo_id FK:
// scope 0 is account-only; positive original repository IDs require current Read
// even after FK SET NULL.
// Neither original scope nor recipient may be adopted or reassigned.
// Lists clamp limit to 1..=100, filter before that limit, and order by created_at
// DESC, id DESC. This contract is not live broadcast/session delivery authority.
// Native SQL transactions retain authority through projection/ack commit, with a
// finite initial ID high-water and bounded candidate pages; SQL errors propagate.
native_operation!(crate::backend::db::postgres::notifications::records::list_notifications_for_user,
    crate::backend::db::sqlite::notifications::records::list_notifications_for_user,
    fn list_notifications_for_user(
    pool: &DbPool,
    user_id: i64,
    limit: i64,
    unread_only: bool,
) -> Result<Vec<Notification>>);

// Unread notifications in the inbox: read_at is NULL and the notification is not done.
native_operation!(crate::backend::db::postgres::notifications::records::count_unread_notifications,
    crate::backend::db::sqlite::notifications::records::count_unread_notifications,
    fn count_unread_notifications(pool: &DbPool, user_id: i64) -> Result<i64>);

// The header's unread count: unread inbox rows the recipient can read, decided in one query.
native_operation!(crate::backend::db::postgres::notifications::records::count_header_unread,
    crate::backend::db::sqlite::notifications::records::count_header_unread,
    fn count_header_unread(pool: &DbPool, user_id: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::notifications::records::mark_notification_read,
    crate::backend::db::sqlite::notifications::records::mark_notification_read,
    fn mark_notification_read(
    pool: &DbPool,
    user_id: i64,
    notification_id: i64,
) -> Result<Option<Notification>>);

native_operation!(crate::backend::db::postgres::notifications::records::mark_all_notifications_read,
    crate::backend::db::sqlite::notifications::records::mark_all_notifications_read,
    fn mark_all_notifications_read(pool: &DbPool, user_id: i64) -> Result<u64>);
