use crate::backend::notifications::NotificationEvent;

use codeyard_db_repos::notifications::events::EventCursor;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio_util::task::AbortOnDropHandle;
#[derive(Serialize, Deserialize)]
struct Announcement {
    origin: String,
    event: NotificationEvent,
}

/// Open notification sockets one account may hold. Each socket answers every event
/// for its account with a projection query, so the number per account is capped.
pub const MAX_SOCKETS_PER_USER: usize = 8;
/// How long a committed change waits for its cross-node hint to be announced.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(2);

type SocketCounts = Arc<std::sync::Mutex<HashMap<i64, usize>>>;
type Subscriptions = Arc<std::sync::Mutex<HashMap<i64, broadcast::Sender<NotificationEvent>>>>;

fn dispatch(subscriptions: &Subscriptions, event: NotificationEvent) {
    let mut subscriptions = subscriptions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(sender) = subscriptions.get(&event.user_id()) {
        if sender.send(event.clone()).is_err() {
            subscriptions.remove(&event.user_id());
        }
    }
}

pub struct NotificationBroadcaster {
    subscriptions: Subscriptions,
    pool: codeyard_db_core::DbPool,
    origin: String,
    stop: watch::Sender<bool>,
    poller: Mutex<Option<AbortOnDropHandle<()>>>,
    sockets: SocketCounts,
}

impl NotificationBroadcaster {
    /// Construction does not start a task. Runtime starts delivery before sharing it.
    pub fn new(pool: &codeyard_db_core::DbPool) -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            subscriptions: Subscriptions::default(),
            pool: pool.clone(),
            origin: uuid::Uuid::new_v4().to_string(),
            stop,
            poller: Mutex::new(None),
            sockets: SocketCounts::default(),
        }
    }

    pub async fn start(&self) -> Result<(), sqlx::Error> {
        let mut task = self.poller.lock().await;
        if task.is_none() && !*self.stop.borrow() {
            let cursor =
                tokio::time::timeout(Duration::from_secs(5), EventCursor::connect(&self.pool))
                    .await
                    .map_err(|_| sqlx::Error::PoolTimedOut)??;
            *task = Some(AbortOnDropHandle::new(tokio::spawn(poll(
                self.pool.clone(),
                self.origin.clone(),
                self.subscriptions.clone(),
                self.stop.subscribe(),
                cursor,
            ))));
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        if let Some(task) = self.poller.lock().await.take() {
            let _ = task.await;
        }
        self.subscriptions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    pub fn subscribe(&self, user_id: i64) -> broadcast::Receiver<NotificationEvent> {
        let mut subscriptions = self
            .subscriptions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        subscriptions.retain(|_, sender| sender.receiver_count() > 0);
        subscriptions
            .entry(user_id)
            .or_insert_with(|| broadcast::channel(1000).0)
            .subscribe()
    }

    /// A slot for one more socket of `user_id`, or `None` when the account already
    /// has `MAX_SOCKETS_PER_USER` open. The slot is released when it is dropped.
    pub fn open_socket(&self, user_id: i64) -> Option<SocketSlot> {
        let mut counts = self
            .sockets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let open = counts.entry(user_id).or_insert(0);
        if *open >= MAX_SOCKETS_PER_USER {
            return None;
        }
        *open += 1;
        Some(SocketSlot {
            user_id,
            counts: self.sockets.clone(),
        })
    }

    /// Delivers `(user_id, notification_id)` rows another transaction stored, such as CI's,
    /// as ordinary creation delivers its own: the announcement and the email.
    pub async fn send_created(&self, notifications: &[(i64, i64)]) {
        for &(user_id, notification_id) in notifications {
            match codeyard_db_repos::notifications::records::get_notification(
                &self.pool,
                user_id,
                notification_id,
            )
            .await
            {
                Ok(Some(notification)) => {
                    crate::backend::notifications::create::deliver(&self.pool, self, &notification)
                        .await;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, notification_id, "Stored notification could not be delivered");
                }
            }
        }
    }

    /// A committed row remains authoritative even if announcing its hint fails. Local sockets
    /// hear first, and the announcement to other nodes is bounded, so a stuck database
    /// connection cannot hold the request that committed the change.
    pub async fn send(&self, event: NotificationEvent) {
        let hint = event.hint();
        dispatch(&self.subscriptions, hint.clone());
        let Ok(payload) = serde_json::to_string(&Announcement {
            origin: self.origin.clone(),
            event: hint,
        }) else {
            return;
        };
        if !matches!(
            tokio::time::timeout(
                ANNOUNCE_TIMEOUT,
                codeyard_db_repos::notifications::events::announce(&self.pool, &payload)
            )
            .await,
            Ok(Ok(()))
        ) {
            tracing::warn!("Notification announcement unavailable");
        }
    }
}

impl Drop for NotificationBroadcaster {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

/// One open notification socket. Dropping it frees the account's slot.
pub struct SocketSlot {
    user_id: i64,
    counts: SocketCounts,
}

impl Drop for SocketSlot {
    fn drop(&mut self) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(open) = counts.get_mut(&self.user_id) {
            *open -= 1;
            if *open == 0 {
                counts.remove(&self.user_id);
            }
        }
    }
}

async fn poll(
    pool: codeyard_db_core::DbPool,
    origin: String,
    subscriptions: Subscriptions,
    mut stop: watch::Receiver<bool>,
    mut cursor: EventCursor,
) {
    let mut wait = Duration::from_millis(50);
    let mut prune_at = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if *stop.borrow() {
            return;
        }
        let received = tokio::select! {
            _ = stop.changed() => return,
            value = tokio::time::timeout(Duration::from_secs(5), cursor.next(&pool)) => value,
        };
        let full = match received {
            Ok(Ok(payloads)) => {
                let full = payloads.len() == codeyard_db_repos::notifications::events::BATCH_SIZE;
                wait = if payloads.is_empty() {
                    (wait * 2).min(Duration::from_millis(500))
                } else {
                    Duration::from_millis(50)
                };
                for payload in payloads {
                    if let Ok(announcement) = serde_json::from_str::<Announcement>(&payload) {
                        if announcement.origin != origin {
                            dispatch(&subscriptions, announcement.event);
                        }
                    }
                }
                full
            }
            _ => {
                tracing::warn!("Notification polling unavailable");
                // A browser must reload after an outage: retained hints may have expired.
                subscriptions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clear();
                loop {
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                    let connected = tokio::select! {
                        _ = stop.changed() => return,
                        value = tokio::time::timeout(Duration::from_secs(5), EventCursor::connect(&pool)) => value,
                    };
                    if let Ok(Ok(connected)) = connected {
                        cursor = connected;
                        // Also close clients that connected while recovery was waiting.
                        subscriptions
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clear();
                        break;
                    }
                }
                false
            }
        };
        if tokio::time::Instant::now() >= prune_at {
            tokio::select! {
                _ = stop.changed() => return,
                result = tokio::time::timeout(Duration::from_secs(5), codeyard_db_repos::notifications::events::prune(&pool)) => {
                    prune_at = match result {
                        Ok(Ok(1024)) => tokio::time::Instant::now(),
                        result => {
                            if !matches!(result, Ok(Ok(_))) {
                                tracing::warn!("Notification hint cleanup unavailable");
                            }
                            tokio::time::Instant::now() + Duration::from_secs(60)
                        }
                    };
                }
            }
        }
        if full {
            continue;
        }
        tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unrelated_recipients_do_not_fill_a_subscription() {
        let pool = codeyard_db_core::DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect_lazy("sqlite::memory:")
                .unwrap(),
        );
        let broadcaster = NotificationBroadcaster::new(&pool);
        let mut alice = broadcaster.subscribe(1);
        let mut bob = broadcaster.subscribe(2);
        dispatch(
            &broadcaster.subscriptions,
            NotificationEvent::UnreadCount {
                user_id: 1,
                unread_count: 3,
            },
        );
        for _ in 0..2000 {
            dispatch(
                &broadcaster.subscriptions,
                NotificationEvent::UnreadCount {
                    user_id: 2,
                    unread_count: 4,
                },
            );
        }
        assert_eq!(alice.recv().await.unwrap().user_id(), 1);
        assert!(matches!(
            alice.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            bob.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }
}
