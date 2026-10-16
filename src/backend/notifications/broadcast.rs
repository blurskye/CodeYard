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
use tokio::task::JoinHandle;
#[derive(Serialize, Deserialize)]
struct Announcement {
    origin: String,
    event: NotificationEvent,
}

/// Open notification sockets one account may hold. Each socket answers every event
/// for its account with a projection query, so the number per account is capped.
pub const MAX_SOCKETS_PER_USER: usize = 8;

type SocketCounts = Arc<std::sync::Mutex<HashMap<i64, usize>>>;

pub struct NotificationBroadcaster {
    tx: broadcast::Sender<NotificationEvent>,
    pool: db::DbPool,
    origin: String,
    stop: watch::Sender<bool>,
    listener: Mutex<Option<JoinHandle<()>>>,
    sockets: SocketCounts,
}

impl NotificationBroadcaster {
    /// Construction does not start a task. Runtime starts delivery before sharing it.
    pub fn new(pool: &db::DbPool) -> Self {
        let (tx, _) = broadcast::channel(1000);
        let (stop, _) = watch::channel(false);
        Self {
            tx,
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
            *task = Some(tokio::spawn(listen(
                self.pool.clone(),
                self.origin.clone(),
                self.tx.clone(),
                self.stop.subscribe(),
            )));
        }
    }

    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        if let Some(task) = self.listener.lock().await.take() {
            let _ = task.await;
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NotificationEvent> {
        self.tx.subscribe()
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

    pub async fn send_created(&self, notifications: &[(i64, i64)]) {
        for &(user_id, notification_id) in notifications {
            self.send(NotificationEvent::Created {
                user_id,
                notification_id,
                kind: String::new(),
                title: String::new(),
                body: None,
                link: None,
                created_at: 0,
                unread_count: 0,
            })
            .await;
        }
    }

    /// A committed row remains authoritative even if announcing its hint fails.
    pub async fn send(&self, event: NotificationEvent) {
        let payload = serde_json::to_string(&Announcement {
            origin: self.origin.clone(),
            event: event.hint(),
        });
        if let Ok(payload) = payload {
            if events::announce(&self.pool, &payload).await.is_err() {
                tracing::warn!("Notification announcement unavailable");
            }
        }
        let _ = self.tx.send(event);
    }
}

impl Drop for NotificationBroadcaster {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.listener.get_mut().take() {
            task.abort();
        }
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
    tx: broadcast::Sender<NotificationEvent>,
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
                Ok(payload) => {
                    if let Ok(announcement) = serde_json::from_str::<Announcement>(&payload) {
                        if announcement.origin != origin {
                            let _ = tx.send(announcement.event);
                        }
                    }
                }
                Err(_) => {
                    tracing::warn!("Notification listener connection lost");
                    break;
                }
            }
        }
    }
}
