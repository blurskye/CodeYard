use crate::models::Notification;
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
