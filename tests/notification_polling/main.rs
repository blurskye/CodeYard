#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "../support/mod.rs"]
mod support;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};

use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, OnceLock,
};
use std::time::Duration;
use support::{database::Database, http_client::Client};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_util::task::AbortOnDropHandle;
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct QueryCount(Arc<AtomicUsize>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for QueryCount {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut cursor_query = false;
        event.record(
            &mut |field: &tracing::field::Field, value: &dyn std::fmt::Debug| {
                if field.name() == "db.statement" {
                    let sql = format!("{value:?}");
                    cursor_query |=
                        sql.contains("FROM notification_events") && !sql.contains("DELETE FROM");
                }
            },
        );
        if cursor_query {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn query_count() -> QueryCount {
    static COUNT: OnceLock<QueryCount> = OnceLock::new();
    COUNT
        .get_or_init(|| {
            let count = QueryCount::default();
            // Global tracing also observes SQLite's dedicated SQL worker threads.
            tracing_subscriber::registry().with(count.clone()).init();
            count
        })
        .clone()
}

async fn frame(socket: &mut TcpStream) -> (u8, Vec<u8>) {
    let opcode = socket.read_u8().await.unwrap() & 0x0f;
    let length = socket.read_u8().await.unwrap();
    assert_eq!(length & 0x80, 0, "server frames are not masked");
    let length = match length {
        126 => u64::from(socket.read_u16().await.unwrap()),
        127 => socket.read_u64().await.unwrap(),
        length => u64::from(length),
    };
    assert!(length <= 65536);
    let mut body = vec![0; length as usize];
    socket.read_exact(&mut body).await.unwrap();
    (opcode, body)
}

async fn connect(address: std::net::SocketAddr, session: &str) -> TcpStream {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("GET /api/v3/notifications/ws HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nCookie: session_id={session}\r\n\r\n").as_bytes()).await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(socket.read_u8().await.unwrap());
            assert!(headers.len() < 8192);
        }
        assert!(headers.starts_with(b"HTTP/1.1 101"), "{}", String::from_utf8_lossy(&headers));
        // A pong proves the route has entered its receive loop and subscribed.
        let mut ping = vec![0x89, 0x85, 1, 2, 3, 4];
        ping.extend(b"ready".iter().enumerate().map(|(i, byte)| *byte ^ [1,2,3,4][i % 4]));
        socket.write_all(&ping).await.unwrap();
        assert_eq!(frame(&mut socket).await, (10, b"ready".to_vec()));
        socket
    }).await.expect("notification socket did not become ready")
}

async fn comment_reaches_connected_clients(pg: bool) {
    let _serial = SERIAL.lock().await;
    let queries = query_count();
    let mut database = Database::new(pg).await;
    database.complete_setup().await;
    let alice = database.actor("alice").await;
    let bob = database.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_app::repositories::lifecycle::provision(
        (database.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let issue = codeyard_db_repos::issues::create_issue(
        database.pool(),
        repo.id,
        alice,
        "A live issue",
        "",
    )
    .await
    .unwrap();
    let state = AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let broadcaster = state.notification_broadcaster.clone();
    broadcaster.start().await.unwrap();
    broadcaster.start().await.unwrap(); // Starting twice still owns one poller.
    let router = create_router(state);
    // The producer has no local subscribers: delivery must cross the database cursor.
    let producer = database.router(root.path()).await;
    let commenter = Client::login(&producer, database.pool(), bob).await;
    let token = tokens::create(
        database.pool(),
        bob,
        Some(&commenter.session),
        TokenInput {
            name: "comment",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let served = router.clone();
    let server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, served)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    }));
    let mut sockets = Vec::new();
    for account in 0..13 {
        let user = if account == 0 {
            alice
        } else {
            database.actor(&format!("reader{account}")).await
        };
        let client = Client::login(&router, database.pool(), user).await;
        for _ in 0..8 {
            if sockets.len() == 100 {
                break;
            }
            sockets.push((user, connect(address, &client.session).await));
        }
    }
    assert_eq!(sockets.len(), 100);
    // Let the idle interval reach its 500 ms cap. There must be no client polling.
    tokio::time::sleep(Duration::from_secs(1)).await;
    queries.0.store(0, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let idle_queries = queries.0.load(Ordering::SeqCst);
    assert!(
        (1..=3).contains(&idle_queries),
        "100 idle clients issued {idle_queries} cursor queries"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    tokio::time::timeout_at(deadline, async {
        let response = producer
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/v3/repos/alice/proj/issues/{}/comments",
                        issue.number
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {}", token.value))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({"body":"A live comment"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        for (user, socket) in &mut sockets {
            if *user != alice {
                continue;
            }
            let (opcode, payload) = frame(socket).await;
            assert_eq!(opcode, 1);
            let event: Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(event["type"], "created");
            assert_eq!(event["kind"], "issue_comment");
            assert_eq!(event["body"], "A live comment");
            assert_eq!(event["unread_count"], 1);
        }
    })
    .await
    .expect("comment did not reach the real notification route within two seconds");

    tokio::time::timeout(Duration::from_secs(1), broadcaster.shutdown())
        .await
        .unwrap();
    // Retaining the broadcaster must not retain its task or any open socket.
    for (_, socket) in &mut sockets {
        let closed = tokio::time::timeout(Duration::from_secs(1), socket.read_u8())
            .await
            .unwrap();
        assert!(closed.is_err(), "shutdown left a socket open");
    }
    let stopped_queries = queries.0.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        queries.0.load(Ordering::SeqCst),
        stopped_queries,
        "poller survived shutdown"
    );
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    database.finish().await;
}

#[tokio::test]
async fn sqlite_comment_reaches_100_client_node_with_one_poller() {
    comment_reaches_connected_clients(false).await;
}

#[tokio::test]
async fn postgres_comment_reaches_100_client_node_with_one_poller() {
    comment_reaches_connected_clients(true).await;
}

async fn cursor_batches_and_retention(pg: bool) {
    let _serial = SERIAL.lock().await;
    let mut database = Database::new(pg).await;
    let pool = database.pool();
    use codeyard_db_repos::notifications::events::{self, EventCursor, BATCH_SIZE};
    let mut cursor = EventCursor::connect(pool).await.unwrap();
    assert!(events::announce(pool, &"x".repeat(4097)).await.is_err());
    for index in 0..=BATCH_SIZE {
        events::announce(pool, &index.to_string()).await.unwrap();
    }
    let first = cursor.next(pool).await.unwrap();
    assert_eq!(first.len(), BATCH_SIZE);
    assert_eq!(first[0], "0");
    assert_eq!(cursor.next(pool).await.unwrap(), [BATCH_SIZE.to_string()]);
    assert!(cursor.next(pool).await.unwrap().is_empty());
    let old = if pg {
        "UPDATE notification_events SET created_at=clock_timestamp()-INTERVAL '2 hours'"
    } else {
        "UPDATE notification_events SET created_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now','-2 hours')"
    };
    database.execute(old, &[]).await;
    database
        .execute(
            "WITH RECURSIVE old(id) AS (SELECT -1 UNION ALL SELECT id-1 FROM old WHERE id>-1025)
         INSERT INTO notification_events(id,payload,created_at)
         SELECT id,'expired','2000-01-01T00:00:00Z' FROM old",
            &[],
        )
        .await;
    events::announce(pool, "fresh").await.unwrap();
    let before = database
        .scalar("SELECT COUNT(*) FROM notification_events", &[])
        .await;
    events::prune(pool).await.unwrap();
    assert_eq!(
        database
            .scalar("SELECT COUNT(*) FROM notification_events", &[])
            .await,
        before - 1024
    );
    events::prune(pool).await.unwrap();
    assert_eq!(
        database
            .scalar("SELECT COUNT(*) FROM notification_events", &[])
            .await,
        1
    );
    assert_eq!(cursor.next(pool).await.unwrap(), ["fresh"]);
    database.finish().await;
}

#[tokio::test]
async fn sqlite_cursor_batches_and_retention() {
    cursor_batches_and_retention(false).await;
}

#[tokio::test]
async fn postgres_cursor_batches_and_retention() {
    cursor_batches_and_retention(true).await;
}

#[tokio::test]
async fn postgres_cursor_replays_late_commits_without_delaying_newer_hints() {
    let _serial = SERIAL.lock().await;
    let mut database = Database::new(true).await;
    use codeyard_db_repos::notifications::events::{announce, EventCursor};
    let mut older = database.pg().begin().await.unwrap();
    sqlx::query("INSERT INTO notification_events (payload) VALUES ('older')")
        .execute(&mut *older)
        .await
        .unwrap();
    // Startup must retain transactions already in flight, too.
    let mut cursor = EventCursor::connect(database.pool()).await.unwrap();
    announce(database.pool(), "newer").await.unwrap();
    assert_eq!(cursor.next(database.pool()).await.unwrap(), ["newer"]);
    assert!(cursor.next(database.pool()).await.unwrap().is_empty());
    older.commit().await.unwrap();
    assert_eq!(cursor.next(database.pool()).await.unwrap(), ["older"]);
    assert!(cursor.next(database.pool()).await.unwrap().is_empty());
    database.finish().await;
}
