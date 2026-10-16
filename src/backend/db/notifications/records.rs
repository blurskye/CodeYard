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

/// Read decisions for one transaction. Each repository is authorized once, when a
/// query first reaches it; a scope missing from `readable` has not been decided yet.
pub(crate) struct NotificationScopes {
    pub high_water: i64,
    pub readable: std::collections::BTreeMap<Option<i64>, bool>,
}

impl NotificationScopes {
    /// Repositories already decided unreadable. Candidate queries skip them, so hidden
    /// history is never read again batch by batch.
    pub(crate) fn unreadable(&self) -> Vec<i64> {
        self.readable
            .iter()
            .filter(|(_, readable)| !**readable)
            .filter_map(|(scope, _)| *scope)
            .collect()
    }
}

/// Original-session persisted projection/acknowledgment. None means the original
/// session is invalid, revoked, expired or its recipient is restricted. Valid
/// actions may return no rows; SQL errors propagate. All result fields share one
/// native transaction, including final expiry verification before commit.
pub enum NotificationInboxAction {
    List {
        limit: i64,
        unread_only: bool,
        before: Option<NotificationCursor>,
    },
    Read {
        notification_id: i64,
    },
    ReadAll,
}

pub struct NotificationInbox {
    pub notifications: Vec<Notification>,
    pub next_cursor: Option<NotificationCursor>,
    pub unread_count: i64,
    /// Unread rows acknowledged by this action; repeated Read acknowledges zero.
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
// NULL/invalid history is quarantined; explicit native zero is account-only;
// positive original repository IDs require current Read even after FK SET NULL.
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

native_operation!(crate::backend::db::postgres::notifications::records::count_unread_notifications,
    crate::backend::db::sqlite::notifications::records::count_unread_notifications,
    fn count_unread_notifications(pool: &DbPool, user_id: i64) -> Result<i64>);

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
