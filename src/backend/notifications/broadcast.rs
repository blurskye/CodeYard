use super::NotificationEvent;
use crate::backend::db;
use crate::backend::db::notifications::events;
use crate::backend::db::notifications::events::NotificationListener;
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
    pool: db::DbPool,
    origin: String,
    stop: watch::Sender<bool>,
    listener: Mutex<Option<AbortOnDropHandle<()>>>,
    sockets: SocketCounts,
}

impl NotificationBroadcaster {
    /// Construction does not start a task. Runtime starts delivery before sharing it.
    pub fn new(pool: &db::DbPool) -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            subscriptions: Subscriptions::default(),
            pool: pool.clone(),
            origin: uuid::Uuid::new_v4().to_string(),
            stop,
            listener: Mutex::new(None),
            sockets: SocketCounts::default(),
        }
    }

    pub async fn start(&self) {
        let mut task = self.listener.lock().await;
        if matches!(&self.pool, db::DbPool::Postgres(_)) && task.is_none() {
            *task = Some(AbortOnDropHandle::new(tokio::spawn(listen(
                self.pool.clone(),
                self.origin.clone(),
                self.subscriptions.clone(),
                self.stop.subscribe(),
            ))));
        }
    }

    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        if let Some(task) = self.listener.lock().await.take() {
            let _ = task.await;
        }
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
            match db::notifications::records::get_notification(&self.pool, user_id, notification_id)
                .await
            {
                Ok(Some(notification)) => {
                    super::create::deliver(&self.pool, self, &notification).await;
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
            tokio::time::timeout(ANNOUNCE_TIMEOUT, events::announce(&self.pool, &payload)).await,
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

async fn listen(
    pool: db::DbPool,
    origin: String,
    subscriptions: Subscriptions,
    mut stop: watch::Receiver<bool>,
) {
    let mut wait = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let connected = tokio::select! {
            _ = stop.changed() => return,
            value = NotificationListener::connect(&pool) => value,
        };
        let mut listener = match connected {
            Ok(Some(listener)) => {
                wait = Duration::from_secs(1);
                // Hints sent while no listener was connected are lost. Dropping every
                // subscription closes the open sockets, and their clients reload the inbox.
                subscriptions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clear();
                listener
            }
            Ok(None) => return,
            Err(_) => {
                tracing::warn!("Notification listener connection unavailable");
                tokio::select! { _ = stop.changed() => return, _ = tokio::time::sleep(wait) => {} }
                wait = (wait * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        loop {
            let received =
                tokio::select! { _ = stop.changed() => return, value = listener.recv() => value };
            match received {
                Ok(Some(payload)) => {
                    if let Ok(announcement) = serde_json::from_str::<Announcement>(&payload) {
                        if announcement.origin != origin {
                            dispatch(&subscriptions, announcement.event);
                        }
                    }
                }
                Ok(None) | Err(_) => {
                    tracing::warn!("Notification listener connection lost");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unrelated_recipients_do_not_fill_a_subscription() {
        let pool = db::DbPool::Sqlite(
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
