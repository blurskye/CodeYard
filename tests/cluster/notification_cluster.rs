//! A notification event sent on one node reaches the browsers connected to the
//! other nodes. Needs PostgreSQL (GITSERV_TEST_DATABASE_URL).
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use codeyard::backend::{
    db::DbPool,
    notifications::{broadcast::MAX_SOCKETS_PER_USER, NotificationBroadcaster, NotificationEvent},
};
use std::time::Duration;
use tokio::sync::broadcast;

/// Sends from `from` until `to` hears it, and returns what `to` heard. A listener that connects
/// closes the subscriptions made before it, as after any gap, so a closed one is made again, as
/// a browser reconnects.
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

#[tokio::test]
async fn an_event_reaches_the_other_node_and_is_not_echoed_back() {
    let url = std::env::var("GITSERV_TEST_DATABASE_URL")
        .expect("GITSERV_TEST_DATABASE_URL must point at a test PostgreSQL database");
    let pool = DbPool::from(sqlx::PgPool::connect(&url).await.unwrap());
    let (a, b) = (
        NotificationBroadcaster::new(&pool),
        NotificationBroadcaster::new(&pool),
    );
    a.start().await;
    b.start().await;
    // Ids no other test uses: every test database node hears the same channel.
    let user_id = 9_000_000_001;
    let heard = hear(&a, &b, user_id).await;
    // Other nodes get a hint; they read the content and the count from the database.
    assert!(matches!(
        heard,
        NotificationEvent::ReadAll { user_id: id, unread_count: 0 } if id == user_id
    ));
    // Node a's listener is connected once it hears node b, so later subscriptions stay open.
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
    pool.close().await;
}

#[tokio::test]
async fn sqlite_delivery_needs_no_remote_listener() {
    let pool = DbPool::from(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    let broadcaster = NotificationBroadcaster::new(&pool);
    broadcaster.start().await;
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
    pool.close().await;
}

#[tokio::test]
async fn an_account_holds_a_bounded_number_of_sockets() {
    let pool = DbPool::from(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
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
