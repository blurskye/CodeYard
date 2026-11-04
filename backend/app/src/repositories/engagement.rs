//! Repository star/watch changes and explicit best-effort notices.

use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_notifications::create::create_and_broadcast_notification;
use codeyard_notifications::create::NotificationRequest;
use serde_json::json;

#[derive(Debug)]
pub enum EngagementError {
    Access(codeyard_identity::repository_access::AccessError),
    Unavailable,
    ChangedCountUnavailable,
}

fn mutation_error(error: anyhow::Error) -> EngagementError {
    if error.is::<codeyard_db_identity::users::blocks::InteractionBlocked>()
        || error.is::<codeyard_support::credentials::AccountRestricted>()
    {
        EngagementError::Access(codeyard_identity::repository_access::AccessError::WriteDenied)
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
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    level: &str,
    events: &[&str],
) -> Result<(), EngagementError> {
    let authority = actor.into();
    let actor = authority.user_id;
    codeyard_identity::repository_access::check_read(pool, repo_id, Some(actor))
        .await
        .map_err(EngagementError::Access)?;
    let saved = match level {
        "participating" => {
            codeyard_db_repos::repositories::engagement::unwatch_repo(pool, authority, repo_id)
                .await
                .map(|_| ())
        }
        "watching" | "ignoring" => {
            codeyard_db_repos::repositories::engagement::set_watch_level(
                pool, authority, repo_id, level, "",
            )
            .await
        }
        "custom" => {
            let events: Vec<&str> = ["issues", "pulls", "releases"]
                .into_iter()
                .filter(|name| events.contains(name))
                .collect();
            codeyard_db_repos::repositories::engagement::set_watch_level(
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
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
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
        codeyard_identity::repository_access::check_read(pool, repo_id, Some(actor))
            .await
            .map_err(EngagementError::Access)?;
        codeyard_db_repos::repositories::engagement::set_watch_level(
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
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    kind: Engagement,
    enabled: bool,
) -> Result<i32, EngagementError> {
    let authority = actor.into();
    let actor = authority.user_id;
    codeyard_identity::repository_access::check_read(pool, repo_id, Some(actor))
        .await
        .map_err(EngagementError::Access)?;
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| EngagementError::Unavailable)?
        .ok_or(EngagementError::Access(
            codeyard_identity::repository_access::AccessError::NotFound,
        ))?;
    let changed = match (kind, enabled) {
        (Engagement::Star, true) => {
            codeyard_db_repos::repositories::engagement::star_repo(pool, authority, repo_id).await
        }
        (Engagement::Star, false) => {
            codeyard_db_repos::repositories::engagement::unstar_repo(pool, authority, repo_id).await
        }
        (Engagement::Watch, true) => {
            codeyard_db_repos::repositories::engagement::watch_repo(pool, authority, repo_id).await
        }
        (Engagement::Watch, false) => {
            codeyard_db_repos::repositories::engagement::unwatch_repo(pool, authority, repo_id)
                .await
        }
    }
    .map_err(mutation_error)?;
    let updated = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo_id)
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
        crate::webhooks::emit(
            pool,
            &updated,
            codeyard_model::webhook_events::STAR,
            vec![actor],
            move |owner, people| {
                let starrer = people.get(actor);
                codeyard_webhooks::payloads::star(action, &starrer, &repository_for_hook, owner)
            },
        )
        .await;
    }
    if let Some(recipient) = updated.personal_owner_id().filter(|_| enabled && changed) {
        if let Some(owner) =
            codeyard_db_repos::repositories::records::repository_owner_name(pool, repo_id)
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
            codeyard_notifications::create::notify_subscribers(
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
