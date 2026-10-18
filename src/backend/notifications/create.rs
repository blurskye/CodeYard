use super::NotificationBroadcaster;
use crate::backend::db;
use crate::backend::db::notifications::records;
pub struct NotificationRequest<'a> {
    pub user_id: i64,
    pub actor_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub kind: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub link: Option<&'a str>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("Notification storage is temporarily unavailable")]
    Unavailable,
    /// The recipient ignores the repository, so nothing is created.
    #[error("The recipient ignores this repository")]
    Ignored,
}

pub async fn create_and_broadcast_notification(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    request: NotificationRequest<'_>,
) -> Result<db::models::Notification, CreateError> {
    if let Some(repo_id) = request.repo_id {
        let level = db::repositories::engagement::get_watch_level(pool, request.user_id, repo_id)
            .await
            .map_err(|_| CreateError::Unavailable)?;
        if level.is_some_and(|(level, _)| level == "ignoring") {
            return Err(CreateError::Ignored);
        }
    }
    let notification = records::create_notification(
        pool,
        records::NewNotification {
            user_id: request.user_id,
            actor_id: request.actor_id,
            repo_id: request.repo_id,
            kind: request.kind,
            title: request.title,
            body: request.body,
            link: request.link,
            metadata: &request.metadata,
        },
    )
    .await
    .map_err(|_| CreateError::Unavailable)?;
    // Delivery reloads the count under the recipient's current authority.
    broadcaster
        .send(super::NotificationEvent::Created {
            user_id: request.user_id,
            notification_id: notification.id,
            kind: notification.kind.clone(),
            title: notification.title.clone(),
            body: notification.body.clone(),
            link: notification.link.clone(),
            created_at: notification.created_at.timestamp(),
            unread_count: 0,
        })
        .await;
    Ok(notification)
}
