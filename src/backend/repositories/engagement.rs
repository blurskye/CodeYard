//! Repository star/watch changes and explicit best-effort notices.
use crate::backend::{
    db,
    notifications::{
        create::{create_and_broadcast_notification, NotificationRequest},
        NotificationBroadcaster,
    },
    repositories::access,
};
use serde_json::json;

#[derive(Debug)]
pub enum EngagementError {
    Access(access::AccessError),
    Unavailable,
    ChangedCountUnavailable,
}

#[derive(Clone, Copy)]
pub enum Engagement {
    Star,
    Watch,
}

pub async fn change(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    repo_id: i64,
    kind: Engagement,
    enabled: bool,
) -> Result<i32, EngagementError> {
    let repository = access::check_read(pool, repo_id, Some(actor))
        .await
        .map_err(EngagementError::Access)?;
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| EngagementError::Unavailable)?
        .ok_or(EngagementError::Access(access::AccessError::NotFound))?;
    let changed = match (kind, enabled) {
        (Engagement::Star, true) => {
            db::repositories::engagement::star_repo(pool, actor, repo_id).await
        }
        (Engagement::Star, false) => {
            db::repositories::engagement::unstar_repo(pool, actor, repo_id).await
        }
        (Engagement::Watch, true) => {
            db::repositories::engagement::watch_repo(pool, actor, repo_id).await
        }
        (Engagement::Watch, false) => {
            db::repositories::engagement::unwatch_repo(pool, actor, repo_id).await
        }
    }
    .map_err(|_| EngagementError::Unavailable)?;
    let updated = db::repositories::records::get_repository_by_id(pool, repo_id)
        .await
        .map_err(|_| EngagementError::ChangedCountUnavailable)?
        .ok_or(EngagementError::ChangedCountUnavailable)?;
    let (count, event, verb) = match kind {
        Engagement::Star => (updated.stars_count, "repository_starred", "starred"),
        Engagement::Watch => (
            updated.watchers_count,
            "repository_watched",
            "is now watching",
        ),
    };
    // Star notifications historically occur on repeated star requests as well.
    if let Some(recipient) = repository
        .personal_owner_id()
        .filter(|id| enabled && *id != actor && (matches!(kind, Engagement::Star) || changed))
    {
        if let Some(owner) = db::repositories::records::repository_owner_name(pool, repo_id)
            .await
            .ok()
            .flatten()
        {
            let title = format!("{} {verb} {}/{}", user.username, owner, repository.name);
            let body = match kind {
                Engagement::Star => format!("Total stars: {count}"),
                Engagement::Watch => format!("Total watchers: {count}"),
            };
            let metadata = match kind {
                Engagement::Star => {
                    json!({"repo_owner": owner, "repo_name": repository.name, "stars_count": count})
                }
                Engagement::Watch => {
                    json!({"repo_owner": owner, "repo_name": repository.name, "watchers_count": count})
                }
            };
            let _ = create_and_broadcast_notification(
                pool,
                broadcaster,
                NotificationRequest {
                    user_id: recipient,
                    actor_id: Some(actor),
                    repo_id: Some(repo_id),
                    kind: event,
                    title: &title,
                    body: Some(&body),
                    link: Some(&format!("/{}/{}", owner, repository.name)),
                    metadata,
                },
            )
            .await;
        }
    }
    Ok(count)
}
