//! Watch notifications. A new issue, pull request or published release notifies the users watching
//! the repository: everyone on "All activity", and "Custom" watchers who picked that kind of event.
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
    Release,
}

impl WatchEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::Pull => "pulls",
            Self::Release => "releases",
        }
    }
}

/// The new issue, pull request or published release a watch notification is about.
pub struct WatchedItem<'a> {
    pub event: WatchEvent,
    pub actor: &'a db::models::User,
    pub repository: &'a db::models::Repository,
    pub owner: &'a str,
    pub repo: &'a str,
    /// The issue or pull request number; `None` for a release.
    pub number: Option<i32>,
    /// The item's title, or the tag of a release.
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
    let (kind, headline) = match (item.event, item.number) {
        (WatchEvent::Issue, Some(number)) => ("issue_opened", format!("opened issue #{number}")),
        (WatchEvent::Pull, Some(number)) => (
            "pull_request_opened",
            format!("opened pull request #{number}"),
        ),
        _ => (
            "release_published",
            format!("published release {}", item.title),
        ),
    };
    let title = format!(
        "{} {headline} in {}/{}",
        item.actor.username, item.owner, item.repo
    );
    // A release notification carries its tag in the title, so the body is left empty.
    let body = (item.event != WatchEvent::Release).then_some(item.title);
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
                    body,
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
