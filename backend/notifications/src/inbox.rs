use crate::NotificationBroadcaster;
use crate::NotificationEvent;

use codeyard_db_repos::notifications::records::NotificationInbox;
use codeyard_db_repos::notifications::records::NotificationInboxAction;
#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("A current notification credential is required")]
    Denied,
    #[error("Notification storage is temporarily unavailable")]
    Unavailable,
}

/// The original session and recipient are checked inside the native transaction.
pub async fn inbox(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    session_id: &str,
    user_id: i64,
    action: NotificationInboxAction,
) -> Result<NotificationInbox, InboxError> {
    with_credential(
        pool,
        broadcaster,
        codeyard_db_repos::notifications::records::NotificationCredential::Session(session_id),
        user_id,
        action,
    )
    .await
}

/// Personal-token access uses the same native transaction, not a browser session.
pub async fn token_inbox(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    token_id: i64,
    user_id: i64,
    action: NotificationInboxAction,
) -> Result<NotificationInbox, InboxError> {
    with_credential(
        pool,
        broadcaster,
        codeyard_db_repos::notifications::records::NotificationCredential::Token(token_id),
        user_id,
        action,
    )
    .await
}
async fn with_credential(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    credential: codeyard_db_repos::notifications::records::NotificationCredential<'_>,
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
        &action,
        NotificationInboxAction::Done { .. } | NotificationInboxAction::DoneAll { .. }
    ) || matches!(
        &action,
        NotificationInboxAction::Threads(
            codeyard_db_repos::notifications::threads::ThreadAction::Read(_)
                | codeyard_db_repos::notifications::threads::ThreadAction::Done(_)
                | codeyard_db_repos::notifications::threads::ThreadAction::ReadAll { .. }
        )
    );
    let result = codeyard_db_repos::notifications::records::notification_inbox_with_credential(
        pool, credential, user_id, action,
    )
    .await
    .map_err(|_| InboxError::Unavailable)?
    .ok_or(InboxError::Denied)?;
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
