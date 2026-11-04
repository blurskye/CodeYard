//! Repository star/watch changes and explicit best-effort notices.
use crate::backend::webhooks::{self, events, payloads};
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

fn mutation_error(error: anyhow::Error) -> EngagementError {
    if error.is::<db::users::blocks::InteractionBlocked>()
        || error.is::<db::users::accounts::AccountRestricted>()
    {
        EngagementError::Access(access::AccessError::WriteDenied)
    } else {
        EngagementError::Unavailable
    }
}

#[derive(Clone, Copy)]
pub enum Engagement {
    Star,
    Watch,
}

/// Saves how the actor watches a repository. `level` is `participating` (the default, no row),
/// `watching`, `custom` or `ignoring`; `events` lists the kinds a `custom` watch wants.
pub async fn set_level(
    pool: &codeyard_db_core::DbPool,
    actor: impl Into<db::credentials::Actor>,
    repo_id: i64,
    level: &str,
    events: &[&str],
) -> Result<(), EngagementError> {
    let authority = actor.into();
    let actor = authority.user_id;
    access::check_read(pool, repo_id, Some(actor))
        .await
        .map_err(EngagementError::Access)?;
    let saved = match level {
        "participating" => db::repositories::engagement::unwatch_repo(pool, authority, repo_id)
            .await
            .map(|_| ()),
        "watching" | "ignoring" => {
            db::repositories::engagement::set_watch_level(pool, authority, repo_id, level, "").await
        }
        "custom" => {
            let events: Vec<&str> = ["issues", "pulls", "releases"]
                .into_iter()
                .filter(|name| events.contains(name))
                .collect();
            db::repositories::engagement::set_watch_level(
                pool,
                authority,
                repo_id,
                "custom",
                &events.join(","),
            )
            .await
        }
        _ => return Err(EngagementError::Unavailable),
    };
    saved.map_err(mutation_error)
}

/// A REST subscription keeps an explicit participating preference and its creation time.
pub async fn set_subscription(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: impl Into<db::credentials::Actor>,
    repo_id: i64,
    subscribed: bool,
    ignored: bool,
) -> Result<(), EngagementError> {
    let authority = actor.into();
    let actor = authority.user_id;
    if subscribed && !ignored {
        change(
            pool,
            broadcaster,
            authority,
            repo_id,
            Engagement::Watch,
            true,
        )
        .await?;
    } else {
        access::check_read(pool, repo_id, Some(actor))
            .await
            .map_err(EngagementError::Access)?;
        db::repositories::engagement::set_watch_level(
            pool,
            authority,
            repo_id,
            if ignored { "ignoring" } else { "participating" },
            "",
        )
        .await
        .map_err(mutation_error)?;
    }
    Ok(())
}

pub async fn change(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: impl Into<db::credentials::Actor>,
    repo_id: i64,
    kind: Engagement,
    enabled: bool,
) -> Result<i32, EngagementError> {
    let authority = actor.into();
    let actor = authority.user_id;
    access::check_read(pool, repo_id, Some(actor))
        .await
        .map_err(EngagementError::Access)?;
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| EngagementError::Unavailable)?
        .ok_or(EngagementError::Access(access::AccessError::NotFound))?;
    let changed = match (kind, enabled) {
        (Engagement::Star, true) => {
            db::repositories::engagement::star_repo(pool, authority, repo_id).await
        }
        (Engagement::Star, false) => {
            db::repositories::engagement::unstar_repo(pool, authority, repo_id).await
        }
        (Engagement::Watch, true) => {
            db::repositories::engagement::watch_repo(pool, authority, repo_id).await
        }
        (Engagement::Watch, false) => {
            db::repositories::engagement::unwatch_repo(pool, authority, repo_id).await
        }
    }
    .map_err(mutation_error)?;
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
    if changed && matches!(kind, Engagement::Star) {
        let action = if enabled { "created" } else { "deleted" };
        let repository_for_hook = updated.clone();
        webhooks::emit(
            pool,
            &updated,
            events::STAR,
            vec![actor],
            move |owner, people| {
                let starrer = people.get(actor);
                payloads::star(action, &starrer, &repository_for_hook, owner)
            },
        )
        .await;
    }
    if let Some(recipient) = updated.personal_owner_id().filter(|_| enabled && changed) {
        if let Some(owner) = db::repositories::records::repository_owner_name(pool, repo_id)
            .await
            .ok()
            .flatten()
        {
            let title = format!("{} {verb} {}/{}", user.username, owner, updated.name);
            let body = match kind {
                Engagement::Star => format!("Total stars: {count}"),
                Engagement::Watch => format!("Total watchers: {count}"),
            };
            let metadata = match kind {
                Engagement::Star => {
                    json!({"repo_owner": owner, "repo_name": updated.name, "stars_count": count, "subject": format!("{owner}/{}", updated.name)})
                }
                Engagement::Watch => {
                    json!({"repo_owner": owner, "repo_name": updated.name, "watchers_count": count, "subject": format!("{owner}/{}", updated.name)})
                }
            };
            let link = codeyard_webui::repository_path(&owner, &updated.name);
            let notice = NotificationRequest {
                user_id: recipient,
                actor_id: Some(actor),
                repo_id: Some(repo_id),
                kind: event,
                title: &title,
                body: Some(&body),
                link: Some(&link),
                metadata,
            };
            let mut notified = Vec::new();
            if recipient != actor
                && create_and_broadcast_notification(pool, broadcaster, notice.clone())
                    .await
                    .is_ok()
            {
                notified.push(recipient);
            }
            crate::backend::notifications::create::notify_subscribers(
                pool,
                broadcaster,
                notice,
                &notified,
            )
            .await;
        }
    }
    Ok(count)
}
