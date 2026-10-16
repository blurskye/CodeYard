use super::NotificationEvent;
use crate::backend::db;
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("Notification session no longer authorizes delivery")]
    Denied,
    #[error("Notification delivery is temporarily unavailable")]
    Unavailable,
}

/// Broadcasts are wakeups, never payload or count authority. Native projection
/// binds the original still-live session and current repository Read at commit.
pub async fn current_event(
    pool: &db::DbPool,
    session_id: &str,
    user_id: i64,
    event: &NotificationEvent,
) -> Result<Option<NotificationEvent>, DeliveryError> {
    if event.user_id() != user_id {
        return Ok(None);
    }
    let id = match event {
        NotificationEvent::Created {
            notification_id, ..
        }
        | NotificationEvent::Read {
            notification_id, ..
        } => Some(*notification_id),
        NotificationEvent::ReadAll { .. } => None,
    };
    let projection =
        db::notifications::records::notification_delivery(pool, session_id, user_id, id)
            .await
            .map_err(|_| DeliveryError::Unavailable)?
            .ok_or(DeliveryError::Denied)?;
    let unread_count = projection.unread_count;
    Ok(match event {
        NotificationEvent::Created { .. } => {
            projection.notification.map(|n| NotificationEvent::Created {
                user_id,
                notification_id: n.id,
                kind: n.kind,
                title: n.title,
                body: n.body,
                link: n.link,
                created_at: n.created_at.timestamp(),
                unread_count,
            })
        }
        NotificationEvent::Read { .. } => projection
            .notification
            .filter(|n| n.read_at.is_some())
            .map(|n| NotificationEvent::Read {
                user_id,
                notification_id: n.id,
                unread_count,
            }),
        NotificationEvent::ReadAll { .. } => Some(NotificationEvent::ReadAll {
            user_id,
            unread_count,
        }),
    })
}
