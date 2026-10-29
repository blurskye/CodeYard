use super::NotificationBroadcaster;
use crate::backend::db;
use crate::backend::db::notifications::records;
/// The invitation stands even if its notification cannot be stored.
pub async fn organization_invitation(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    invitation: &db::models::OrgInvitation,
) {
    let name =
        match db::organizations::get_organization_metadata(pool, actor, invitation.org_id).await {
            Ok(db::organizations::OrganizationOutcome::Applied(metadata)) => metadata.name,
            _ => return,
        };
    if let Some(user_id) = invitation.user_id {
        let request = NotificationRequest {
            user_id,
            actor_id: Some(actor),
            repo_id: None,
            kind: "organization_invitation",
            title: &format!("Invitation to join {name}"),
            body: None,
            link: Some("/settings/organizations"),
            metadata: serde_json::json!({ "org_id": invitation.org_id, "role": invitation.role }),
        };
        if let Err(error) = create_and_broadcast_notification(pool, broadcaster, request).await {
            tracing::warn!(%error, "organization invitation notification was not stored");
        }
    }
    crate::backend::email::organization_invitation(pool, invitation, &name).await;
}

#[derive(Clone)]
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

impl NotificationRequest<'_> {
    fn as_native(&self) -> records::NewNotification<'_> {
        records::NewNotification {
            user_id: self.user_id,
            actor_id: self.actor_id,
            repo_id: self.repo_id,
            kind: self.kind,
            title: self.title,
            body: self.body,
            link: self.link,
            metadata: &self.metadata,
        }
    }
}

/// Explicit subscriptions augment the existing native recipients once per event; default
/// threads retain the native recipient policy. The ordinary creation path applies ignores.
pub async fn notify_subscribers(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    request: NotificationRequest<'_>,
    notified: &[i64],
) -> Vec<i64> {
    let Some((kind, key)) = db::notifications::threads::subject(&request.as_native()) else {
        return Vec::new();
    };
    let Some(repo) = request.repo_id else {
        return Vec::new();
    };
    let recipients = match db::notifications::threads::subscribers(pool, repo, kind, &key).await {
        Ok(recipients) => recipients,
        Err(error) => {
            tracing::warn!(%error,"Thread subscriber lookup failed after the native mutation committed");
            return Vec::new();
        }
    };
    let mut sent = Vec::new();
    for user in recipients {
        if request.actor_id == Some(user) || notified.contains(&user) {
            continue;
        }
        if request.kind == "repository_forked" {
            let Some(fork) = request.metadata["fork_repository_id"].as_i64() else {
                continue;
            };
            match crate::backend::repositories::access::check_read(pool, fork, Some(user)).await {
                Ok(_) => {}
                Err(crate::backend::repositories::access::AccessError::Unavailable) => {
                    tracing::warn!(user_id = user, "Fork notification access unavailable");
                    continue;
                }
                Err(_) => continue,
            }
        }
        let mut notice = request.clone();
        notice.user_id = user;
        match create_and_broadcast_notification(pool, broadcaster, notice).await {
            Ok(_) => sent.push(user),
            Err(CreateError::Unavailable) => tracing::warn!(
                user_id = user,
                "Thread notification storage unavailable after the native mutation committed"
            ),
            Err(CreateError::Ignored | CreateError::Suppressed) => {}
        }
    }
    sent
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("Notification storage is temporarily unavailable")]
    Unavailable,
    /// The recipient ignores the repository, so nothing is created.
    #[error("The recipient ignores this repository")]
    Ignored,
    #[error("Notification is no longer needed")]
    Suppressed,
}

pub async fn create_and_broadcast_notification(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    request: NotificationRequest<'_>,
) -> Result<db::models::Notification, CreateError> {
    if let Some(repo_id) = request.repo_id {
        // A repository notice carries its content into the inbox and the email, so the
        // recipient must still be able to read the repository.
        use crate::backend::repositories::access::{check_read, AccessError};
        match check_read(pool, repo_id, Some(request.user_id)).await {
            Ok(_) => {}
            Err(AccessError::Unavailable) => return Err(CreateError::Unavailable),
            Err(_) => return Err(CreateError::Suppressed),
        }
        let level = db::repositories::engagement::get_watch_level(pool, request.user_id, repo_id)
            .await
            .map_err(|_| CreateError::Unavailable)?;
        if level.is_some_and(|(level, _)| level == "ignoring") {
            return Err(CreateError::Ignored);
        }
    }
    let notification = records::create_notification(pool, request.as_native())
        .await
        .map_err(|error| {
            if error.is::<records::NotificationSuppressed>() {
                CreateError::Suppressed
            } else {
                CreateError::Unavailable
            }
        })?;
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
    // Repeated low-value social toggles are not worth an email.
    if !matches!(
        notification.kind.as_str(),
        "user_followed" | "repository_starred" | "repository_watched" | "organization_invitation"
    ) {
        crate::backend::email::notify_user(pool, &notification).await;
    }
    Ok(notification)
}
