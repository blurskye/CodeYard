use super::NotificationBroadcaster;
use super::NotificationEvent;
use crate::backend::db;
use crate::backend::db::notifications::records;
use crate::backend::db::notifications::records::NotificationInbox;
use crate::backend::db::notifications::records::NotificationInboxAction;
#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("A current notification session is required")]
    Denied,
    #[error("Notification storage is temporarily unavailable")]
    Unavailable,
}

/// The original session and recipient are checked inside the native transaction.
pub async fn inbox(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    session_id: &str,
    user_id: i64,
    action: NotificationInboxAction,
) -> Result<NotificationInbox, InboxError> {
    let read = match &action {
        NotificationInboxAction::Read { notification_id } => Some(Some(*notification_id)),
        NotificationInboxAction::ReadAll => Some(None),
        _ => None,
    };
    // Marking done takes a notification out of the unread count, so it is announced too.
    let marks_done = matches!(
        action,
        NotificationInboxAction::Done { .. } | NotificationInboxAction::DoneAll { .. }
    );
    let mut result = records::notification_inbox(pool, session_id, user_id, action)
        .await
        .map_err(|_| InboxError::Unavailable)?
        .ok_or(InboxError::Denied)?;
    records::refresh_links(pool, &mut result.notifications)
        .await
        .map_err(|_| InboxError::Unavailable)?;
    match read {
        Some(Some(notification_id)) if !result.notifications.is_empty() => {
            broadcaster
                .send(NotificationEvent::Read {
                    user_id,
                    notification_id,
                    unread_count: result.unread_count,
                })
                .await
        }
        Some(None) => {
            broadcaster
                .send(NotificationEvent::ReadAll {
                    user_id,
                    unread_count: result.unread_count,
                })
                .await
        }
        _ => {}
    }
    if marks_done && result.updated > 0 {
        broadcaster
            .send(NotificationEvent::UnreadCount {
                user_id,
                unread_count: result.unread_count,
            })
            .await;
    }
    Ok(result)
}
