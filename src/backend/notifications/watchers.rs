//! Watch notifications. A new issue or pull request notifies the users watching the repository:
//! everyone on "All activity", and "Custom" watchers who picked that kind of event.
use super::create::{create_and_broadcast_notification, NotificationRequest};
use super::NotificationBroadcaster;
use crate::backend::db;
use serde_json::json;

/// Watchers read per query. A full page means more may follow, so the reads continue.
const WATCHER_BATCH: i64 = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WatchEvent {
    Issue,
    Pull,
}

impl WatchEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::Pull => "pulls",
        }
    }
}

/// The new issue or pull request a watch notification is about.
pub struct WatchedItem<'a> {
    pub event: WatchEvent,
    pub actor: &'a db::models::User,
    pub repository: &'a db::models::Repository,
    pub owner: &'a str,
    pub repo: &'a str,
    pub number: i32,
    pub title: &'a str,
    pub link: &'a str,
    /// Users this event already notifies for another reason. Each one is notified once.
    pub notified: &'a [i64],
}

/// Notifies the watchers of a new item. The write has already been saved, so a failure here
/// never fails the request. Returns the users it notified, so a mention does not notify them again.
pub async fn notify_watchers(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    item: WatchedItem<'_>,
) -> Vec<i64> {
    let (kind, noun) = match item.event {
        WatchEvent::Issue => ("issue_opened", "issue"),
        WatchEvent::Pull => ("pull_request_opened", "pull request"),
    };
    let title = format!(
        "{} opened {noun} #{} in {}/{}",
        item.actor.username, item.number, item.owner, item.repo
    );
    let mut sent = Vec::new();
    let mut after = 0;
    loop {
        // The query applies the event, ignore, account and read rules, so every user it returns
        // is notified.
        let batch = match db::repositories::engagement::list_event_watchers(
            pool,
            item.repository.id,
            item.event.name(),
            after,
            WATCHER_BATCH,
        )
        .await
        {
            Ok(batch) => batch,
            Err(error) => {
                tracing::warn!(%error, "Watch notifications stopped early: watcher lookup failed");
                return sent;
            }
        };
        let Some(&last) = batch.last() else {
            break;
        };
        after = last;
        let more = batch.len() as i64 == WATCHER_BATCH;
        for user_id in batch {
            if user_id == item.actor.id || item.notified.contains(&user_id) {
                continue;
            }
            let _ = create_and_broadcast_notification(
                pool,
                broadcaster,
                NotificationRequest {
                    user_id,
                    actor_id: Some(item.actor.id),
                    repo_id: Some(item.repository.id),
                    kind,
                    title: &title,
                    body: Some(item.title),
                    link: Some(item.link),
                    metadata: json!({
                        "repo_owner": item.owner,
                        "repo_name": item.repo,
                        "number": item.number,
                        "subject": item.title,
                    }),
                },
            )
            .await;
            sent.push(user_id);
        }
        if !more {
            break;
        }
    }
    sent
}
