//! Notification hints cross web nodes through a bounded database cursor.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database::Database;
use codeyard::backend::notifications::broadcast::MAX_SOCKETS_PER_USER;
use codeyard::backend::notifications::NotificationBroadcaster;
use codeyard::backend::notifications::NotificationEvent;
use codeyard_db_core::DbPool;
use std::time::Duration;
use tokio::sync::broadcast;

/// Sends from `from` until `to` hears it, reconnecting after a database outage.
async fn hear(
    from: &NotificationBroadcaster,
    to: &NotificationBroadcaster,
    user_id: i64,
) -> NotificationEvent {
    let event = NotificationEvent::ReadAll {
        user_id,
        unread_count: 3,
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut receiver = to.subscribe(user_id);
        loop {
            from.send(event.clone()).await;
            match tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
                Ok(Ok(heard)) => return heard,
                Ok(Err(broadcast::error::RecvError::Closed)) => receiver = to.subscribe(user_id),
                _ => {}
            }
        }
    })
    .await
    .expect("the other node never heard the event")
}

async fn cross_node_delivery(pg: bool) {
    let mut database = Database::new(pg).await;
    let pool = database.pool().clone();
    let (a, b) = (
        NotificationBroadcaster::new(&pool),
        NotificationBroadcaster::new(&pool),
    );
    a.start().await.unwrap();
    b.start().await.unwrap();
    let user_id = 9_000_000_001;
    let heard = hear(&a, &b, user_id).await;
    // Other nodes get a hint; they read the content and the count from the database.
    assert!(matches!(
        heard,
        NotificationEvent::ReadAll { user_id: id, unread_count: 0 } if id == user_id
    ));
    // Another account keeps node b's extra sends away from the count below.
    hear(&b, &a, 9_000_000_002).await;

    // Node a's own subscribers get each event once, not again from the announcement.
    let mut on_a = a.subscribe(user_id);
    let sent = 3;
    for _ in 0..sent {
        a.send(NotificationEvent::ReadAll {
            user_id,
            unread_count: 3,
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut local = 0;
    while let Ok(event) = on_a.try_recv() {
        if event.user_id() == user_id {
            local += 1;
        }
    }
    assert_eq!(local, sent);
    a.shutdown().await;
    b.shutdown().await;
    database.finish().await;
}

#[tokio::test]
async fn sqlite_cross_node_delivery_without_echo() {
    cross_node_delivery(false).await;
}

#[tokio::test]
async fn postgres_cross_node_delivery_without_echo() {
    cross_node_delivery(true).await;
}

#[tokio::test]
async fn sqlite_local_delivery_and_poller_shutdown() {
    let mut database = Database::new(false).await;
    let pool = database.pool().clone();
    let broadcaster = NotificationBroadcaster::new(&pool);
    broadcaster.start().await.unwrap();
    let mut receiver = broadcaster.subscribe(42);
    broadcaster
        .send(NotificationEvent::ReadAll {
            user_id: 42,
            unread_count: 7,
        })
        .await;
    let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    // Local subscribers receive hints too; delivery reloads the authoritative count.
    assert!(matches!(
        event,
        NotificationEvent::ReadAll {
            user_id: 42,
            unread_count: 0
        }
    ));
    tokio::time::timeout(Duration::from_secs(1), broadcaster.shutdown())
        .await
        .unwrap();
    assert!(matches!(
        receiver.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
    database.finish().await;
}

#[tokio::test]
async fn an_account_holds_a_bounded_number_of_sockets() {
    let pool =
        codeyard_db_core::DbPool::from(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    let broadcaster = NotificationBroadcaster::new(&pool);
    let slots: Vec<_> = (0..MAX_SOCKETS_PER_USER)
        .map(|_| broadcaster.open_socket(42).expect("a slot below the limit"))
        .collect();
    assert!(
        broadcaster.open_socket(42).is_none(),
        "the socket past the limit is refused"
    );
    assert!(
        broadcaster.open_socket(43).is_some(),
        "other accounts keep their slots"
    );
    drop(slots);
    assert!(
        broadcaster.open_socket(42).is_some(),
        "closed sockets free their slots"
    );
    pool.close().await;
}
