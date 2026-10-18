//! Notifications are persisted authority; broadcasts are hints only.
use serde::Deserialize;
use serde::Serialize;

pub mod assignments;
pub mod broadcast;
pub mod create;
pub mod delivery;
pub mod inbox;
pub mod mentions;
pub mod reasons;
pub mod watchers;

pub use broadcast::NotificationBroadcaster;
pub use delivery::current_event;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationEvent {
    Created {
        user_id: i64,
        notification_id: i64,
        kind: String,
        title: String,
        body: Option<String>,
        link: Option<String>,
        created_at: i64,
        unread_count: i64,
    },
    Read {
        user_id: i64,
        notification_id: i64,
        unread_count: i64,
    },
    ReadAll {
        user_id: i64,
        unread_count: i64,
    },
    /// The unread count changed without a read: a notification was marked done or moved back.
    UnreadCount {
        user_id: i64,
        unread_count: i64,
    },
}

impl NotificationEvent {
    pub fn user_id(&self) -> i64 {
        match self {
            Self::Created { user_id, .. }
            | Self::Read { user_id, .. }
            | Self::ReadAll { user_id, .. }
            | Self::UnreadCount { user_id, .. } => *user_id,
        }
    }

    /// Only which notification changed. A node that receives this from another node
    /// reads the content and the unread count from the database, like every event.
    fn hint(&self) -> Self {
        match self {
            Self::Created {
                user_id,
                notification_id,
                ..
            } => Self::Created {
                user_id: *user_id,
                notification_id: *notification_id,
                kind: String::new(),
                title: String::new(),
                body: None,
                link: None,
                created_at: 0,
                unread_count: 0,
            },
            Self::Read {
                user_id,
                notification_id,
                ..
            } => Self::Read {
                user_id: *user_id,
                notification_id: *notification_id,
                unread_count: 0,
            },
            Self::ReadAll { user_id, .. } => Self::ReadAll {
                user_id: *user_id,
                unread_count: 0,
            },
            Self::UnreadCount { user_id, .. } => Self::UnreadCount {
                user_id: *user_id,
                unread_count: 0,
            },
        }
    }
}
