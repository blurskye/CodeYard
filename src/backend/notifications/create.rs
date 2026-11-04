use crate::backend::notifications::NotificationBroadcaster;

/// The invitation stands even if its notification cannot be stored. Its email was queued in the
/// transaction that saved it.
pub async fn organization_invitation(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    invitation: &codeyard_db_core::models::OrgInvitation,
) {
    let Some(user_id) = invitation.user_id else {
        return;
    };
    let name = match codeyard_db_orgs::organizations::get_organization_metadata(
        pool,
        actor,
        invitation.org_id,
    )
    .await
    {
        Ok(codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(metadata)) => {
            metadata.name
        }
        _ => return,
    };
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
    fn as_native(&self) -> codeyard_db_core::queries::notifications::records::NewNotification<'_> {
        codeyard_db_core::queries::notifications::records::NewNotification {
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
/// threads retain the native recipient policy. The ordinary creation path applies ignores
/// and, for a fork notice, Read on the fork.
pub async fn notify_subscribers(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    request: NotificationRequest<'_>,
    notified: &[i64],
) -> Vec<i64> {
    let Some((kind, key)) =
        codeyard_db_core::queries::notifications::threads::subject(&request.as_native())
    else {
        return Vec::new();
    };
    let Some(repo) = request.repo_id else {
        return Vec::new();
    };
    let recipients = match codeyard_db_repos::notifications::threads::subscribers(
        pool, repo, kind, &key,
    )
    .await
    {
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
        let mut notice = request.clone();
        notice.user_id = user;
        match create_and_broadcast_notification(pool, broadcaster, notice).await {
            Ok(_) => sent.push(user),
            Err(CreateError::Unavailable) => tracing::warn!(
                user_id = user,
                "Thread notification storage unavailable after the native mutation committed"
            ),
            Err(CreateError::Suppressed) => {}
        }
    }
    sent
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("Notification storage is temporarily unavailable")]
    Unavailable,
    /// Blocked, unreadable, ignored or already sent, so nothing is created.
    #[error("Notification is no longer needed")]
    Suppressed,
}

pub async fn create_and_broadcast_notification(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    request: NotificationRequest<'_>,
) -> Result<codeyard_db_core::models::Notification, CreateError> {
    if let Some(repo_id) = request.repo_id {
        // A repository notice carries its content into the inbox and the email, so the
        // recipient must still be able to read the repository.
        use crate::backend::repositories::access::check_read;
        use crate::backend::repositories::access::AccessError;
        match check_read(pool, repo_id, Some(request.user_id)).await {
            Ok(_) => {}
            Err(AccessError::Unavailable) => return Err(CreateError::Unavailable),
            Err(_) => return Err(CreateError::Suppressed),
        }
    }
    // The insert rechecks Read, the fork's Read and repository ignores in its transaction.
    let notification = codeyard_db_repos::notifications::records::create_notification(
        pool,
        request.as_native(),
    )
    .await
    .map_err(|error| {
        if error.is::<codeyard_db_core::queries::notifications::records::NotificationSuppressed>() {
            CreateError::Suppressed
        } else {
            CreateError::Unavailable
        }
    })?;
    deliver(pool, broadcaster, &notification).await;
    Ok(notification)
}

/// Announces a committed notification and queues its email. Every stored notification is
/// delivered here, whichever transaction wrote it.
pub(super) async fn deliver(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    notification: &codeyard_db_core::models::Notification,
) {
    // Delivery reloads the count under the recipient's current authority.
    broadcaster
        .send(crate::backend::notifications::NotificationEvent::Created {
            user_id: notification.user_id,
            notification_id: notification.id,
            kind: notification.kind.clone(),
            title: notification.title.clone(),
            body: notification.body.clone(),
            link: notification.link.clone(),
            created_at: notification.created_at.timestamp(),
            unread_count: 0,
        })
        .await;
    // Repeated low-value social toggles are not worth an email. An invitation queues its own
    // email with the offer.
    if !matches!(
        notification.kind.as_str(),
        "user_followed" | "repository_starred" | "repository_watched" | "organization_invitation"
    ) {
        crate::backend::email::notify_user(pool, notification).await;
    }
}
