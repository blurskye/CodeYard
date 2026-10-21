//! Uploaded files on a real SQLite database and a real state directory, through
//! the domain functions and through the HTTP routes. No S3 is involved; the S3
//! side of uploads is tested in `production_storage.rs`.
use axum::{
    body::Body,
    http::{header, request::Builder, Request, StatusCode},
    response::Response,
    Router,
};
use codeyard::{
    backend::{
        api::create_router,
        app::AppState,
        db::{
            self, files as files_db,
            models::{FileReason, FileVisibility},
        },
        files::{
            self, ConfirmationError, DeleteError, DownloadCapability, DownloadError,
            DownloadTransport, LocalUploadError, UploadCapability, UploadDeclaration, UploadError,
        },
        identity::avatars::{self, AvatarError},
        storage::{local::LocalStore, s3::S3Config, ObjectStore, Upload},
    },
    webui::{
        app::{Page, PageEnvelope},
        pages::admin::storage as storage_view,
    },
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Cursor,
    os::unix::fs::{symlink, MetadataExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn stalled_object_cleanup_is_concurrent_bounded_and_keeps_queued_work() {
    let f = Fixture::open().await;
    for index in 0..100 {
        sqlx::query("INSERT INTO pending_object_deletions(object_key,delete_after) VALUES(?,?)")
            .bind(format!("uploads/stalled-{index}"))
            .bind(two_days_ago())
            .execute(&f.raw)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO pending_object_deletions(object_key,delete_after) VALUES('uploads/healthy',?)",
    )
    .bind(timestamp(chrono::Utc::now() - chrono::Duration::days(1)))
    .execute(&f.raw)
    .await
    .unwrap();
    let healthy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let healthy_count = healthy.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let storage = Arc::new(
        ObjectStore::open(
            f.temporary.path(),
            Some(S3Config {
                endpoint,
                public_endpoint: None,
                access_key: "test".into(),
                secret_key: "test-secret".into(),
                bucket: "uploads".into(),
                region: "us-east-1".into(),
            }),
        )
        .await
        .unwrap(),
    );
    let (sent, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let sent = sent.clone();
            let healthy = healthy_count.clone();
            connections.spawn(async move {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                if String::from_utf8_lossy(&bytes[..count]).contains("uploads/healthy") {
                    healthy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                } else {
                    let _ = sent.send(());
                    std::future::pending::<()>().await;
                }
            });
        }
    });
    let pool = f.pool.clone();
    let sweeping = storage.clone();
    let cleanup =
        tokio::spawn(async move { files::remove_expired_uploads(&pool, &sweeping).await });
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), requests.recv())
            .await
            .is_err()
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(8), cleanup)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        0
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_object_deletions")
        .fetch_one(&f.raw)
        .await
        .unwrap();
    assert_eq!(count, 101);
    // Timed-out and unattempted keys have backoff, so later healthy work is reachable.
    files::remove_expired_uploads(&f.pool, &storage)
        .await
        .unwrap();
    assert_eq!(healthy.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(files_db::due_object_deletions(&f.pool, 100)
        .await
        .unwrap()
        .is_empty());
    // Successful S3 deletion keeps its tombstone, allowing a later PUT to be reconciled.
    sqlx::query(
        "UPDATE pending_object_deletions SET delete_after=? WHERE object_key='uploads/healthy'",
    )
    .bind(two_days_ago())
    .execute(&f.raw)
    .await
    .unwrap();
    files::remove_expired_uploads(&f.pool, &storage)
        .await
        .unwrap();
    assert_eq!(healthy.load(std::sync::atomic::Ordering::SeqCst), 2);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn confirmations_refuse_a_busy_file_instead_of_queueing_behind_a_put() {
    let f = Fixture::open().await;
    let reservation = f
        .reserve(
            b"hello",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let file = files_db::get_file_by_id(&f.pool, reservation.file_id)
        .await
        .unwrap()
        .unwrap();
    let lock = f.storage.lock_file(&file.object_key).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), f.confirm(reservation.file_id))
        .await
        .unwrap();
    assert_eq!(result, Err(ConfirmationError::StorageUnavailable));
    drop(lock);
    f.put(&reservation, b"hello", "text/plain").await.unwrap();
    f.confirm(reservation.file_id).await.unwrap();
}

#[tokio::test]
async fn put_waiters_take_io_slots_before_waiting_on_the_file_lock() {
    let f = Fixture::open().await;
    let reservation = f
        .reserve(
            b"hello",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let file = files_db::get_file_by_id(&f.pool, reservation.file_id)
        .await
        .unwrap()
        .unwrap();
    let _lock = f.storage.lock_file(&file.object_key).await.unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let (pool, store, owner, id) = (f.pool.clone(), f.storage.clone(), f.owner, file.id);
        tasks.spawn(async move {
            files::put_local_upload(
                &pool,
                &store,
                owner,
                id,
                "text/plain",
                std::io::Cursor::new(b"hello"),
            )
            .await
        });
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.storage.try_acquire_io().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            files::put_local_upload(
                &f.pool,
                &f.storage,
                f.owner,
                file.id,
                "text/plain",
                std::io::Cursor::new(b"hello")
            )
        )
        .await
        .unwrap(),
        Err(LocalUploadError::StorageUnavailable)
    );
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// SQLite stores times as text in this shape.
fn timestamp(time: chrono::DateTime<chrono::Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

fn two_days_ago() -> String {
    timestamp(chrono::Utc::now() - chrono::Duration::days(2))
}

/// A browser session: the cookie value and the CSRF token its pages carry.
struct Login {
    session: String,
    csrf: String,
}

async fn login(pool: &db::DbPool, user: i64, session_fill: char, csrf_fill: char) -> Login {
    let session = session_fill.to_string().repeat(64);
    let csrf = csrf_fill.to_string().repeat(64);
    db::users::sessions::create_session(
        pool,
        &session,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(
        db::users::sessions::get_or_create_csrf_token(pool, &session, Some(user), &csrf, 3600)
            .await
            .unwrap(),
        Some(csrf.clone())
    );
    Login { session, csrf }
}

struct Fixture {
    temporary: tempfile::TempDir,
    raw: sqlx::SqlitePool,
    pool: db::DbPool,
    storage: Arc<ObjectStore>,
    owner: i64,
    stranger: i64,
    login: Login,
    stranger_login: Login,
}

impl Fixture {
    async fn open() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let raw = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(temporary.path().join("application.sqlite"))
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                    .busy_timeout(Duration::from_secs(5)),
            )
            .await
            .unwrap();
        let pool: db::DbPool = raw.clone().into();
        db::run_migrations(&pool)
            .await
            .expect("native SQLite migrations are required, not success-skipped");
        let owner =
            sqlx::query_scalar("INSERT INTO users(username) VALUES ('local_owner') RETURNING id")
                .fetch_one(&raw)
                .await
                .unwrap();
        let stranger = sqlx::query_scalar(
            "INSERT INTO users(username) VALUES ('local_stranger') RETURNING id",
        )
        .fetch_one(&raw)
        .await
        .unwrap();
        sqlx::query("INSERT INTO node_configuration(id,setup_completed) VALUES (1,1) ON CONFLICT(id) DO UPDATE SET setup_completed=1")
            .execute(&raw)
            .await
            .unwrap();
        // The tests below set their own limits where they matter.
        sqlx::query("UPDATE file_reason_limits SET max_uploads_per_hour=1000,max_uploads_per_day=1000,max_bytes_per_hour=1048576000,max_bytes_per_day=1048576000")
            .execute(&raw)
            .await
            .unwrap();
        let login_owner = login(&pool, owner, 'a', 'b').await;
        let login_stranger = login(&pool, stranger, 'c', 'd').await;
        let storage = Arc::new(
            ObjectStore::open(&temporary.path().join("state"), None)
                .await
                .unwrap(),
        );
        Self {
            temporary,
            raw,
            pool,
            storage,
            owner,
            stranger,
            login: login_owner,
            stranger_login: login_stranger,
        }
    }

    fn state_path(&self) -> PathBuf {
        self.temporary.path().join("state")
    }

    fn object_path(&self, file_id: Uuid) -> PathBuf {
        self.state_path()
            .join("objects/uploads")
            .join(file_id.to_string())
    }

    /// Names of everything in the upload directory and in the staging directory that
    /// holds temporary files.
    fn stored_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for directory in ["objects/uploads", "objects/.staging"] {
            let Ok(entries) = std::fs::read_dir(self.state_path().join(directory)) else {
                continue;
            };
            names.extend(
                entries.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()),
            );
        }
        names.sort();
        names
    }

    async fn declare(
        &self,
        declaration: UploadDeclaration<'_>,
    ) -> Result<files::UploadReservation, UploadError> {
        files::reserve_and_sign_upload(&self.pool, &self.storage, self.owner, declaration).await
    }

    async fn try_reserve(
        &self,
        bytes: &[u8],
        kind: &str,
        reason: FileReason,
        visibility: FileVisibility,
    ) -> Result<files::UploadReservation, UploadError> {
        self.declare(UploadDeclaration {
            filename: "owned.bin",
            content_type: kind,
            file_size: bytes.len() as i64,
            reason,
            visibility,
            sha256_hex: &digest(bytes),
        })
        .await
    }

    async fn reserve(
        &self,
        bytes: &[u8],
        kind: &str,
        reason: FileReason,
        visibility: FileVisibility,
    ) -> files::UploadReservation {
        self.try_reserve(bytes, kind, reason, visibility)
            .await
            .unwrap()
    }

    async fn put(
        &self,
        reservation: &files::UploadReservation,
        bytes: &[u8],
        kind: &str,
    ) -> Result<(), LocalUploadError> {
        files::put_local_upload(
            &self.pool,
            &self.storage,
            self.owner,
            reservation.file_id,
            kind,
            Cursor::new(bytes),
        )
        .await
    }

    async fn confirm(&self, file_id: Uuid) -> Result<(), ConfirmationError> {
        files::confirm_upload(&self.pool, &self.storage, self.owner, file_id).await
    }

    /// Reserves, uploads and confirms a file for the owner.
    async fn store(
        &self,
        bytes: &[u8],
        kind: &str,
        reason: FileReason,
        visibility: FileVisibility,
    ) -> Uuid {
        let reservation = self.reserve(bytes, kind, reason, visibility).await;
        self.put(&reservation, bytes, kind).await.unwrap();
        self.confirm(reservation.file_id).await.unwrap();
        reservation.file_id
    }

    async fn activate(&self, bytes: &[u8]) -> Uuid {
        self.store(
            bytes,
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await
    }

    /// Ends the upload window of a pending file.
    async fn expire(&self, file_id: Uuid) {
        sqlx::query("UPDATE files SET upload_expires_at=?1 WHERE id=?2")
            .bind(timestamp(chrono::Utc::now() - chrono::Duration::seconds(1)))
            .bind(file_id)
            .execute(&self.raw)
            .await
            .unwrap();
    }

    /// What the application would stream to this actor for the file.
    async fn bytes(&self, file_id: Uuid, actor: Option<i64>) -> Result<Vec<u8>, DownloadError> {
        let asset = files::open_asset_download(&self.pool, &self.storage, actor, file_id).await?;
        let DownloadTransport::ApplicationStream {
            mut reader,
            io_guard,
            ..
        } = asset.transport
        else {
            panic!("local files are streamed by the application")
        };
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        drop(io_guard);
        Ok(bytes)
    }

    fn router(&self) -> Router {
        create_router(AppState::new(
            self.pool.clone(),
            self.temporary.path().join("repos"),
            self.storage.clone(),
            None,
            codeyard::backend::ci::CiConfig::default(),
            None,
        ))
    }
}

async fn call(router: &Router, request: Request<Body>) -> Response {
    router.clone().oneshot(request).await.unwrap()
}

async fn status_of(router: &Router, request: Request<Body>) -> StatusCode {
    call(router, request).await.status()
}

/// Takes every free storage slot and keeps them until the returned guards drop.
fn take_storage_slots(storage: &ObjectStore) -> Vec<tokio::sync::OwnedSemaphorePermit> {
    let mut permits = Vec::new();
    while let Ok(permit) = storage.try_acquire_io() {
        permits.push(permit);
    }
    permits
}

/// How many storage slots are free right now, found by taking them all.
fn free_slots(storage: &ObjectStore) -> usize {
    take_storage_slots(storage).len()
}

async fn body_of(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn json_of(response: Response) -> Value {
    serde_json::from_slice(&body_of(response).await).unwrap()
}

/// A request as a page of this site would send it: the session cookie and,
/// when `send_csrf` is set, its CSRF token.
fn request(
    method: &str,
    uri: &str,
    login: Option<&Login>,
    send_csrf: bool,
    body: Option<&Value>,
) -> Request<Body> {
    let mut builder: Builder = Request::builder().method(method).uri(uri);
    if let Some(login) = login {
        builder = builder.header(header::COOKIE, format!("session_id={}", login.session));
        if send_csrf {
            builder = builder.header("x-csrf-token", &login.csrf);
        }
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// A PUT of upload bytes to the URL the reservation returned.
fn put_content(uri: &str, login: Option<&Login>, csrf: Option<&str>, body: Body) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(uri)
        .header(header::CONTENT_TYPE, "text/plain");
    if let Some(login) = login {
        builder = builder.header(header::COOKIE, format!("session_id={}", login.session));
    }
    if let Some(csrf) = csrf {
        builder = builder.header("x-csrf-token", csrf);
    }
    builder.body(body).unwrap()
}

#[tokio::test]
async fn stream_confirm_download_delete_and_reopen() {
    let fixture = Fixture::open().await;
    let bytes = vec![b'x'; 256 * 1024];
    let reservation = fixture
        .reserve(
            &bytes,
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let id = reservation.file_id;
    assert_eq!(
        reservation.capability,
        UploadCapability::AuthenticatedLocalPut
    );
    assert_eq!(reservation.method, "PUT");
    assert_eq!(reservation.expires_in, 900);
    assert_eq!(reservation.url, format!("/api/files/{id}/content"));
    assert_eq!(reservation.object_key, format!("uploads/{id}"));
    assert_eq!(reservation.headers["content-type"], "text/plain");
    assert_eq!(
        reservation.headers["content-length"],
        bytes.len().to_string()
    );

    // Until the bytes are confirmed the file is only a pending record.
    let pending = files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .unwrap();
    assert!(!pending.is_active && pending.confirmed_at.is_none());
    assert!(pending.upload_expires_at.is_some());
    assert_eq!(pending.object_key, reservation.object_key);
    assert_eq!(pending.sha256_hex, digest(&bytes));
    assert!(!fixture.object_path(id).exists());

    // The body is read as a stream, not collected first.
    let (reader, mut writer) = tokio::io::duplex(4096);
    let owned = bytes.clone();
    let producer = tokio::spawn(async move {
        for chunk in owned.chunks(1024) {
            writer.write_all(chunk).await.unwrap();
        }
        writer.shutdown().await.unwrap();
    });
    files::put_local_upload(
        &fixture.pool,
        &fixture.storage,
        fixture.owner,
        id,
        "text/plain",
        reader,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    assert_eq!(fixture.stored_names(), vec![id.to_string()]);
    assert_eq!(
        std::fs::metadata(fixture.object_path(id)).unwrap().mode() & 0o077,
        0,
        "other local users must not be able to read uploads"
    );
    for actor in [Some(fixture.owner), None] {
        assert_eq!(
            fixture.bytes(id, actor).await.unwrap_err(),
            DownloadError::NotFound,
            "stored but unconfirmed files are not downloadable"
        );
    }
    assert_eq!(
        files::download_for_user(&fixture.pool, &fixture.storage, fixture.owner, id)
            .await
            .unwrap_err(),
        DownloadError::NotFound
    );

    fixture.confirm(id).await.unwrap();
    let active = files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .unwrap();
    assert!(active.is_active && active.confirmed_at.is_some());
    assert!(active.upload_expires_at.is_none());
    assert_eq!(fixture.bytes(id, Some(fixture.owner)).await.unwrap(), bytes);

    // A restarted server reads the same files.
    let reopened = ObjectStore::open(&fixture.state_path(), None)
        .await
        .unwrap();
    let asset = files::open_asset_download(&fixture.pool, &reopened, Some(fixture.owner), id)
        .await
        .unwrap();
    assert_eq!(asset.filename, "owned.bin");
    assert_eq!(asset.inline_type, None);
    let DownloadTransport::ApplicationStream {
        mut reader,
        bytes: length,
        io_guard,
    } = asset.transport
    else {
        panic!("local files are streamed by the application")
    };
    assert_eq!(length, bytes.len() as i64);
    let mut received = Vec::new();
    reader.read_to_end(&mut received).await.unwrap();
    drop(io_guard);
    assert_eq!(received, bytes);

    let signed = files::download_for_user(&fixture.pool, &fixture.storage, fixture.owner, id)
        .await
        .unwrap();
    assert_eq!(
        signed.capability,
        DownloadCapability::AuthorizedApplicationGet
    );
    assert_eq!(signed.url, format!("/api/files/{id}/content"));
    assert_eq!(signed.filename, "owned.bin");
    assert_eq!(signed.content_type, "text/plain");
    assert_eq!(signed.file_size, bytes.len() as i64);
    assert_eq!(
        signed.expires_in, 0,
        "a local URL is not a time-limited grant"
    );
    let stats = files_db::get_user_file_stats(&fixture.pool, fixture.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.downloads_24h, 1);
    assert_eq!(stats.total_bytes, bytes.len() as i64);

    // Deleting removes both the record and the stored bytes.
    files::delete_owned_file(&fixture.pool, &fixture.storage, fixture.owner, id)
        .await
        .unwrap();
    assert!(fixture.stored_names().is_empty());
    assert!(files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        fixture.bytes(id, Some(fixture.owner)).await.unwrap_err(),
        DownloadError::NotFound
    );
    assert_eq!(
        files::delete_owned_file(&fixture.pool, &fixture.storage, fixture.owner, id)
            .await
            .unwrap_err(),
        DeleteError::NotFound
    );
}

#[tokio::test]
async fn rejected_uploads_leave_nothing_behind_and_keep_the_reservation() {
    let fixture = Fixture::open().await;
    let reservation = fixture
        .reserve(
            b"goodgood",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let id = reservation.file_id;

    // Only the owner may send bytes.
    assert_eq!(
        files::put_local_upload(
            &fixture.pool,
            &fixture.storage,
            fixture.stranger,
            id,
            "text/plain",
            Cursor::new(b"goodgood")
        )
        .await
        .unwrap_err(),
        LocalUploadError::NotFound
    );
    // The content type must be the declared one.
    assert_eq!(
        fixture
            .put(&reservation, b"goodgood", "application/octet-stream")
            .await
            .unwrap_err(),
        LocalUploadError::Invalid
    );
    // Wrong length (short and long) and wrong checksum are refused.
    for bytes in [
        b"short".as_slice(),
        b"longlonglong".as_slice(),
        b"badbytes".as_slice(),
    ] {
        assert_eq!(
            fixture
                .put(&reservation, bytes, "text/plain")
                .await
                .unwrap_err(),
            LocalUploadError::Rejected
        );
    }
    // A page declared as an image is not stored as one.
    let fake = fixture
        .reserve(
            b"notimage",
            "image/png",
            FileReason::Avatar,
            FileVisibility::Public,
        )
        .await;
    assert_eq!(
        fixture
            .put(&fake, b"notimage", "image/png")
            .await
            .unwrap_err(),
        LocalUploadError::Rejected
    );
    assert!(
        fixture.stored_names().is_empty(),
        "refused uploads must not leave files or temporary files"
    );

    // Confirming before the bytes arrive fails, and so does confirming
    // someone else's upload; the reservation stays pending throughout.
    assert_eq!(
        fixture.confirm(id).await.unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    assert_eq!(
        files::confirm_upload(&fixture.pool, &fixture.storage, fixture.stranger, id)
            .await
            .unwrap_err(),
        ConfirmationError::NotFound
    );

    // The right bytes still go through afterwards.
    fixture
        .put(&reservation, b"goodgood", "text/plain")
        .await
        .unwrap();
    fixture.confirm(id).await.unwrap();
    assert_eq!(
        fixture.bytes(id, Some(fixture.owner)).await.unwrap(),
        b"goodgood"
    );
    assert_eq!(
        fixture.confirm(id).await.unwrap_err(),
        ConfirmationError::NotPending
    );
}

#[tokio::test]
async fn declarations_the_policy_refuses_are_never_recorded() {
    let fixture = Fixture::open().await;
    let sha = digest(b"x");
    let upper = sha.to_uppercase();
    let declaration = || UploadDeclaration {
        filename: "owned.bin",
        content_type: "text/plain",
        file_size: 1,
        reason: FileReason::Other,
        visibility: FileVisibility::Owner,
        sha256_hex: &sha,
    };
    // The limit for "other" is 10 MiB: exactly that is fine.
    fixture
        .declare(UploadDeclaration {
            file_size: 10 * 1024 * 1024,
            ..declaration()
        })
        .await
        .unwrap();
    let refused = [
        (
            "empty file",
            UploadDeclaration {
                file_size: 0,
                ..declaration()
            },
        ),
        (
            "negative size",
            UploadDeclaration {
                file_size: -1,
                ..declaration()
            },
        ),
        (
            "larger than the reason allows",
            UploadDeclaration {
                file_size: 10 * 1024 * 1024 + 1,
                ..declaration()
            },
        ),
        (
            "larger than any upload",
            UploadDeclaration {
                file_size: i64::MAX,
                ..declaration()
            },
        ),
        (
            "empty name",
            UploadDeclaration {
                filename: "",
                ..declaration()
            },
        ),
        (
            "NUL in the name",
            UploadDeclaration {
                filename: "a\0b",
                ..declaration()
            },
        ),
        (
            "no content type",
            UploadDeclaration {
                content_type: "",
                ..declaration()
            },
        ),
        (
            "content type without a subtype",
            UploadDeclaration {
                content_type: "text",
                ..declaration()
            },
        ),
        (
            "short checksum",
            UploadDeclaration {
                sha256_hex: "abcd",
                ..declaration()
            },
        ),
        (
            "upper-case checksum",
            UploadDeclaration {
                sha256_hex: &upper,
                ..declaration()
            },
        ),
        (
            "public file that is not an avatar",
            UploadDeclaration {
                visibility: FileVisibility::Public,
                ..declaration()
            },
        ),
        (
            "avatar that is not an image",
            UploadDeclaration {
                reason: FileReason::Avatar,
                ..declaration()
            },
        ),
    ];
    for (name, refused) in refused {
        assert_eq!(
            fixture.declare(refused).await.unwrap_err(),
            UploadError::Invalid,
            "{name}"
        );
    }
    let records: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM files), (SELECT COUNT(*) FROM upload_rate_limit)",
    )
    .fetch_one(&fixture.raw)
    .await
    .unwrap();
    assert_eq!(records, (1, 1), "only the accepted declaration is recorded");
}

#[tokio::test]
async fn upload_quotas_count_reservations_and_reset_with_time() {
    let fixture = Fixture::open().await;
    sqlx::query("UPDATE file_reason_limits SET max_uploads_per_hour=2,max_bytes_per_hour=10 WHERE reason='other'")
        .execute(&fixture.raw)
        .await
        .unwrap();
    let reserve = |bytes: &'static [u8]| {
        fixture.try_reserve(
            bytes,
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
    };
    let first = reserve(b"four").await.unwrap();
    // The bytes budget for the hour is 10 and 4 are used.
    assert_eq!(reserve(b"seven!!").await.unwrap_err(), UploadError::Quota);
    let second = reserve(b"sixsix").await.unwrap();
    assert_eq!(reserve(b"x").await.unwrap_err(), UploadError::Quota);
    // Two uploads per hour have been made, however many bytes are left.
    sqlx::query("UPDATE file_reason_limits SET max_bytes_per_hour=1000000 WHERE reason='other'")
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(reserve(b"x").await.unwrap_err(), UploadError::Quota);

    // Each reason and each user has a separate budget.
    fixture
        .reserve(PNG, "image/png", FileReason::Avatar, FileVisibility::Public)
        .await;
    files::reserve_and_sign_upload(
        &fixture.pool,
        &fixture.storage,
        fixture.stranger,
        UploadDeclaration {
            filename: "theirs.bin",
            content_type: "text/plain",
            file_size: 1,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(b"x"),
        },
    )
    .await
    .unwrap();

    // Deleting a reservation or letting it expire does not refund the budget.
    files::delete_owned_file(
        &fixture.pool,
        &fixture.storage,
        fixture.owner,
        first.file_id,
    )
    .await
    .unwrap();
    assert_eq!(reserve(b"x").await.unwrap_err(), UploadError::Quota);
    fixture.expire(second.file_id).await;
    assert_eq!(
        files::remove_expired_uploads(&fixture.pool, &fixture.storage)
            .await
            .unwrap(),
        1
    );
    assert_eq!(reserve(b"x").await.unwrap_err(), UploadError::Quota);

    // Only time does: the budget returns once the uploads leave the window.
    sqlx::query("UPDATE upload_rate_limit SET uploaded_at=?1 WHERE user_id=?2")
        .bind(two_days_ago())
        .bind(fixture.owner)
        .execute(&fixture.raw)
        .await
        .unwrap();
    reserve(b"x").await.unwrap();
}

#[tokio::test]
async fn reservations_recheck_account_state_and_policy() {
    let fixture = Fixture::open().await;
    for restriction in ["is_banned", "must_change_password"] {
        sqlx::query(&format!("UPDATE users SET {restriction}=1 WHERE id=?1"))
            .bind(fixture.owner)
            .execute(&fixture.raw)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .try_reserve(b"x", "text/plain", FileReason::Other, FileVisibility::Owner)
                .await
                .unwrap_err(),
            UploadError::Invalid
        );
        let records: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM files), (SELECT COUNT(*) FROM upload_rate_limit)",
        )
        .fetch_one(&fixture.raw)
        .await
        .unwrap();
        assert_eq!(records, (0, 0), "{restriction}: nothing may be recorded");
        sqlx::query(&format!("UPDATE users SET {restriction}=0 WHERE id=?1"))
            .bind(fixture.owner)
            .execute(&fixture.raw)
            .await
            .unwrap();
    }
    fixture.activate(b"allowed again").await;

    // Without a policy row for the reason, uploads are refused, not unlimited.
    sqlx::query("DELETE FROM file_reason_limits WHERE reason='other'")
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .try_reserve(b"x", "text/plain", FileReason::Other, FileVisibility::Owner)
            .await
            .unwrap_err(),
        UploadError::AdmissionUnavailable
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_uploads_and_confirmations_leave_one_correct_active_file() {
    let fixture = Arc::new(Fixture::open().await);
    let reservation = Arc::new(
        fixture
            .reserve(
                b"immutable",
                "text/plain",
                FileReason::Other,
                FileVisibility::Owner,
            )
            .await,
    );
    let id = reservation.file_id;
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut puts = Vec::new();
    for index in 0..8 {
        let fixture = fixture.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        puts.push(tokio::spawn(async move {
            barrier.wait().await;
            // Every other sender has bytes that do not match the declaration.
            let bytes: &[u8] = if index % 2 == 0 {
                b"immutable"
            } else {
                b"IMMUTABLE"
            };
            fixture.put(&reservation, bytes, "text/plain").await
        }));
    }
    for (index, put) in puts.into_iter().enumerate() {
        let result = put.await.unwrap();
        if index % 2 == 0 {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err(), LocalUploadError::Rejected);
        }
    }
    assert_eq!(fixture.stored_names(), vec![id.to_string()]);
    assert_eq!(
        std::fs::read(fixture.object_path(id)).unwrap(),
        b"immutable"
    );

    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut confirmations = Vec::new();
    for _ in 0..8 {
        let fixture = fixture.clone();
        let barrier = barrier.clone();
        confirmations.push(tokio::spawn(async move {
            barrier.wait().await;
            fixture.confirm(id).await
        }));
    }
    let mut winners = 0;
    for confirmation in confirmations {
        match confirmation.await.unwrap() {
            Ok(()) => winners += 1,
            Err(ConfirmationError::NotPending) => (),
            other => panic!("unexpected confirmation: {other:?}"),
        }
    }
    assert_eq!(winners, 1);

    // Once active, the file takes no more bytes, even matching ones.
    assert_eq!(
        fixture
            .put(&reservation, b"immutable", "text/plain")
            .await
            .unwrap_err(),
        LocalUploadError::NotPending
    );
    assert_eq!(
        fixture.bytes(id, Some(fixture.owner)).await.unwrap(),
        b"immutable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_http_put_serializes_delete_confirm_and_expiry_per_file() {
    let fixture = Fixture::open().await;
    let router = fixture.router();

    let deleted = fixture
        .reserve(
            b"paused!",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let (send, receive) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
    let body = Body::from_stream(futures_util::stream::unfold(receive, |mut receive| async {
        receive.recv().await.map(|frame| (frame, receive))
    }));
    let put_router = router.clone();
    let put_request = put_content(
        &deleted.url,
        Some(&fixture.login),
        Some(&fixture.login.csrf),
        body,
    );
    let put = tokio::spawn(async move { call(&put_router, put_request).await.status() });
    send.send(Ok(bytes::Bytes::from_static(b"pau")))
        .await
        .unwrap();
    for _ in 0..100 {
        if !fixture.stored_names().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !fixture.stored_names().is_empty(),
        "PUT did not start streaming"
    );
    let delete_router = router.clone();
    let delete_request = request(
        "DELETE",
        &format!("/api/files/{}", deleted.file_id),
        Some(&fixture.login),
        true,
        None,
    );
    let delete = tokio::spawn(async move { call(&delete_router, delete_request).await.status() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !delete.is_finished(),
        "DELETE must wait for this file's PUT"
    );
    send.send(Ok(bytes::Bytes::from_static(b"sed!")))
        .await
        .unwrap();
    drop(send);
    assert_eq!(put.await.unwrap(), StatusCode::NO_CONTENT);
    assert_eq!(delete.await.unwrap(), StatusCode::NO_CONTENT);
    assert!(files_db::get_file_by_id(&fixture.pool, deleted.file_id)
        .await
        .unwrap()
        .is_none());
    assert!(!fixture.object_path(deleted.file_id).exists());

    let confirmed = fixture
        .reserve(
            b"confirm",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let (reader, mut writer) = tokio::io::duplex(64);
    writer.write_all(b"con").await.unwrap();
    let pool = fixture.pool.clone();
    let storage = fixture.storage.clone();
    let owner = fixture.owner;
    let confirmed_id = confirmed.file_id;
    let put = tokio::spawn(async move {
        files::put_local_upload(&pool, &storage, owner, confirmed_id, "text/plain", reader).await
    });
    for _ in 0..100 {
        if fixture
            .object_path(confirmed.file_id)
            .parent()
            .unwrap()
            .exists()
            && !fixture.stored_names().is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pool = fixture.pool.clone();
    let storage = fixture.storage.clone();
    let confirm =
        tokio::spawn(
            async move { files::confirm_upload(&pool, &storage, owner, confirmed_id).await },
        );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !confirm.is_finished(),
        "confirm must wait for this file's PUT"
    );
    writer.write_all(b"firm").await.unwrap();
    writer.shutdown().await.unwrap();
    put.await.unwrap().unwrap();
    confirm.await.unwrap().unwrap();
    assert!(files_db::get_file_by_id(&fixture.pool, confirmed.file_id)
        .await
        .unwrap()
        .is_some_and(|file| file.is_active));
    assert!(fixture.object_path(confirmed.file_id).exists());

    let expired = fixture
        .reserve(
            b"expired",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let (reader, mut writer) = tokio::io::duplex(64);
    writer.write_all(b"exp").await.unwrap();
    let pool = fixture.pool.clone();
    let storage = fixture.storage.clone();
    let expired_id = expired.file_id;
    let put = tokio::spawn(async move {
        files::put_local_upload(&pool, &storage, owner, expired_id, "text/plain", reader).await
    });
    for _ in 0..100 {
        if fixture.stored_names().len() > 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fixture.expire(expired.file_id).await;
    let pool = fixture.pool.clone();
    let storage = fixture.storage.clone();
    let sweep = tokio::spawn(async move { files::remove_expired_uploads(&pool, &storage).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!sweep.is_finished(), "expiry must wait for this file's PUT");
    writer.write_all(b"ired").await.unwrap();
    writer.shutdown().await.unwrap();
    put.await.unwrap().unwrap();
    assert_eq!(sweep.await.unwrap().unwrap(), 1);
    assert!(files_db::get_file_by_id(&fixture.pool, expired.file_id)
        .await
        .unwrap()
        .is_none());
    assert!(!fixture.object_path(expired.file_id).exists());
}

#[tokio::test]
async fn expired_reservations_are_refused_and_swept_with_their_bytes() {
    let fixture = Fixture::open().await;
    let stale = fixture
        .reserve(
            b"stale",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    fixture.put(&stale, b"stale", "text/plain").await.unwrap();
    let unsent = fixture
        .reserve(
            b"unsent",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let live = fixture
        .reserve(
            b"live",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let active = fixture.activate(b"keeper").await;
    fixture.expire(stale.file_id).await;
    fixture.expire(unsent.file_id).await;

    // After the window the reservation takes no bytes and cannot be confirmed.
    assert_eq!(
        fixture
            .put(&stale, b"stale", "text/plain")
            .await
            .unwrap_err(),
        LocalUploadError::NotPending
    );
    assert_eq!(
        fixture.confirm(stale.file_id).await.unwrap_err(),
        ConfirmationError::NotPending
    );

    // The sweep removes expired records and any bytes they have, and nothing else.
    assert_eq!(
        files::remove_expired_uploads(&fixture.pool, &fixture.storage)
            .await
            .unwrap(),
        2
    );
    for gone in [stale.file_id, unsent.file_id] {
        assert!(files_db::get_file_by_id(&fixture.pool, gone)
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(fixture.stored_names(), vec![active.to_string()]);
    assert!(files_db::get_file_by_id(&fixture.pool, live.file_id)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        fixture.bytes(active, Some(fixture.owner)).await.unwrap(),
        b"keeper"
    );
    assert_eq!(
        files::remove_expired_uploads(&fixture.pool, &fixture.storage)
            .await
            .unwrap(),
        0
    );

    // A late sender finds nothing to write to.
    assert_eq!(
        fixture
            .put(&stale, b"stale", "text/plain")
            .await
            .unwrap_err(),
        LocalUploadError::NotFound
    );
    assert_eq!(
        fixture.confirm(stale.file_id).await.unwrap_err(),
        ConfirmationError::NotFound
    );
}

#[tokio::test]
async fn the_expiry_sweep_works_through_a_backlog_in_batches() {
    let fixture = Fixture::open().await;
    for _ in 0..101 {
        fixture
            .reserve(b"x", "text/plain", FileReason::Other, FileVisibility::Owner)
            .await;
    }
    sqlx::query("UPDATE files SET upload_expires_at=?1")
        .bind(timestamp(chrono::Utc::now() - chrono::Duration::minutes(1)))
        .execute(&fixture.raw)
        .await
        .unwrap();
    let mut removed = Vec::new();
    for _ in 0..3 {
        removed.push(
            files::remove_expired_uploads(&fixture.pool, &fixture.storage)
                .await
                .unwrap(),
        );
    }
    assert_eq!(removed, [100, 1, 0]);
}

#[tokio::test]
async fn due_object_cleanup_progresses_during_a_full_expired_batch() {
    let fixture = Fixture::open().await;
    let replayed = fixture.activate(b"queued cleanup").await;
    let replayed_path = fixture.object_path(replayed);
    std::fs::remove_file(&replayed_path).unwrap();
    std::fs::create_dir(&replayed_path).unwrap();
    files::delete_owned_file(&fixture.pool, &fixture.storage, fixture.owner, replayed)
        .await
        .unwrap();
    std::fs::remove_dir(&replayed_path).unwrap();
    std::fs::write(&replayed_path, b"replayed after delete").unwrap();
    sqlx::query("UPDATE pending_object_deletions SET delete_after=?1 WHERE object_key=?2")
        .bind(timestamp(chrono::Utc::now() - chrono::Duration::seconds(1)))
        .bind(format!("uploads/{replayed}"))
        .execute(&fixture.raw)
        .await
        .unwrap();

    for _ in 0..100 {
        fixture
            .reserve(b"x", "text/plain", FileReason::Other, FileVisibility::Owner)
            .await;
    }
    sqlx::query("UPDATE files SET upload_expires_at=?1")
        .bind(timestamp(chrono::Utc::now() - chrono::Duration::minutes(1)))
        .execute(&fixture.raw)
        .await
        .unwrap();

    assert_eq!(
        files::remove_expired_uploads(&fixture.pool, &fixture.storage)
            .await
            .unwrap(),
        100
    );
    assert!(
        !replayed_path.exists(),
        "due durable cleanup must not be starved by expired metadata"
    );
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pending_object_deletions WHERE object_key=?1")
            .bind(format!("uploads/{replayed}"))
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(queued, 0);
}

#[tokio::test]
async fn private_files_stay_with_their_owner_and_only_avatars_are_public() {
    let fixture = Fixture::open().await;
    let private = fixture.activate(b"private bytes").await;
    for actor in [None, Some(fixture.stranger)] {
        assert_eq!(
            fixture.bytes(private, actor).await.unwrap_err(),
            DownloadError::NotFound
        );
        assert_eq!(
            files::download_asset(&fixture.pool, &fixture.storage, actor, private)
                .await
                .unwrap_err(),
            DownloadError::NotFound
        );
    }
    assert_eq!(
        files::download_for_user(&fixture.pool, &fixture.storage, fixture.stranger, private)
            .await
            .unwrap_err(),
        DownloadError::NotFound
    );
    // The owner cannot make an ordinary file public.
    assert_eq!(
        fixture
            .try_reserve(
                b"x",
                "text/plain",
                FileReason::Other,
                FileVisibility::Public
            )
            .await
            .unwrap_err(),
        UploadError::Invalid
    );

    // An avatar declared public can be fetched by anyone, logged in or not.
    let avatar = fixture
        .store(PNG, "image/png", FileReason::Avatar, FileVisibility::Public)
        .await;
    for actor in [None, Some(fixture.stranger), Some(fixture.owner)] {
        assert_eq!(fixture.bytes(avatar, actor).await.unwrap(), PNG);
    }
    let asset = files::open_asset_download(&fixture.pool, &fixture.storage, None, avatar)
        .await
        .unwrap();
    assert_eq!(asset.inline_type.as_deref(), Some("image/png"));
    let shown = files::download_asset(&fixture.pool, &fixture.storage, None, avatar)
        .await
        .unwrap();
    assert_eq!(shown.url, format!("/api/files/{avatar}/content"));
    let stats = files_db::get_user_file_stats(&fixture.pool, fixture.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.downloads_24h, 0, "asset reads are not counted");

    // A user avatar or organization avatar in use is public too.
    let quiet = fixture
        .store(PNG, "image/png", FileReason::Avatar, FileVisibility::Owner)
        .await;
    let organization = fixture
        .store(
            PNG,
            "image/png",
            FileReason::OrgAvatar,
            FileVisibility::Owner,
        )
        .await;
    for file in [quiet, organization] {
        assert_eq!(
            fixture.bytes(file, None).await.unwrap_err(),
            DownloadError::NotFound
        );
    }
    sqlx::query("UPDATE users SET avatar_file_id=?1 WHERE id=?2")
        .bind(quiet)
        .bind(fixture.owner)
        .execute(&fixture.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organizations(name,avatar_file_id) VALUES ('local-org',?1)")
        .bind(organization)
        .execute(&fixture.raw)
        .await
        .unwrap();
    for file in [quiet, organization] {
        assert_eq!(fixture.bytes(file, None).await.unwrap(), PNG);
    }
}

#[tokio::test]
async fn object_names_cannot_leave_the_object_directory() {
    let fixture = Fixture::open().await;
    let outside = fixture.temporary.path().join("outside");
    std::fs::write(&outside, b"do not change").unwrap();
    let sha = digest(b"do not change");
    let local = LocalStore::open(&fixture.state_path().join("objects"))
        .await
        .unwrap();
    for key in [
        "../outside",
        "uploads/../../outside",
        "/etc/passwd",
        "uploads//x",
        "uploads/x.y",
        "",
    ] {
        let upload = Upload {
            key,
            bytes: 13,
            content_type: "text/plain",
            sha256_hex: &sha,
        };
        assert!(local.open_file(key).await.is_err(), "open {key:?}");
        assert!(local.delete(key).await.is_err(), "delete {key:?}");
        assert!(local.verify(&upload).await.is_err(), "verify {key:?}");
        assert!(
            local
                .put(&upload, Cursor::new(b"do not change"))
                .await
                .is_err(),
            "put {key:?}"
        );
    }
    assert_eq!(std::fs::read(&outside).unwrap(), b"do not change");
    assert!(fixture.stored_names().is_empty());

    // A link planted where an upload will go is replaced, never written through.
    let reservation = fixture
        .reserve(
            b"payload",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let path = fixture.object_path(reservation.file_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(&outside, &path).unwrap();
    fixture
        .put(&reservation, b"payload", "text/plain")
        .await
        .unwrap();
    assert_eq!(std::fs::read(&outside).unwrap(), b"do not change");
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_file());
    assert_eq!(std::fs::read(&path).unwrap(), b"payload");
}

#[tokio::test]
async fn s3_settings_are_validated_and_never_fall_back_to_local_storage() {
    let state = tempfile::tempdir().unwrap();
    let secret = "must-never-appear-in-errors";
    let invalid = S3Config {
        endpoint: format!("https://user:{secret}@objects.example.test"),
        public_endpoint: None,
        access_key: "private-access".into(),
        secret_key: secret.into(),
        bucket: "owned".into(),
        region: "us-east-1".into(),
    };
    let error = ObjectStore::open(state.path(), Some(invalid))
        .await
        .err()
        .expect("invalid S3 settings must fail, not succeed as local storage");
    assert!(!format!("{error:#}").contains(secret));
    assert!(!state.path().join("objects").exists());

    // Opening an S3 store does not contact the server.
    let valid = S3Config {
        endpoint: "http://127.0.0.1:9".into(),
        public_endpoint: Some("https://files.example.test".into()),
        access_key: "private-access".into(),
        secret_key: secret.into(),
        bucket: "uploads-bucket".into(),
        region: "us-east-1".into(),
    };
    let s3 = ObjectStore::open(state.path(), Some(valid)).await.unwrap();
    assert_eq!(s3.kind(), "s3");
    assert_eq!(s3.location().as_deref(), Some("uploads-bucket"));
    assert!(!state.path().join("objects").exists());

    let local = ObjectStore::open(state.path(), None).await.unwrap();
    assert_eq!(local.kind(), "local");
    assert_eq!(local.location(), None);
}

#[tokio::test]
async fn busy_downloads_are_refused_and_never_block_uploads() {
    let fixture = Arc::new(Fixture::open().await);
    let shared = fixture.activate(b"download").await;
    let reservation = fixture
        .reserve(
            b"retry",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    fixture
        .put(&reservation, b"retry", "text/plain")
        .await
        .unwrap();

    // Open downloads until the node says it is busy; each holds its slot.
    let mut downloads = Vec::new();
    let busy = loop {
        match files::open_asset_download(
            &fixture.pool,
            &fixture.storage,
            Some(fixture.owner),
            shared,
        )
        .await
        {
            Ok(download) => downloads.push(download),
            Err(error) => break error,
        }
        assert!(downloads.len() < 10_000, "downloads are never refused");
    };
    assert_eq!(busy, DownloadError::StorageUnavailable);
    assert!(!downloads.is_empty());

    // Uploads, confirmations and deletes don't wait for downloads.
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.confirm(reservation.file_id),
    )
    .await
    .expect("confirmation does not wait for downloads")
    .unwrap();
    let other = fixture
        .reserve(
            b"other",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    files::delete_owned_file(
        &fixture.pool,
        &fixture.storage,
        fixture.owner,
        other.file_id,
    )
    .await
    .unwrap();

    // A finished download frees its slot for the next one.
    drop(downloads.pop());
    files::open_asset_download(&fixture.pool, &fixture.storage, Some(fixture.owner), shared)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .bytes(reservation.file_id, Some(fixture.owner))
            .await
            .unwrap(),
        b"retry"
    );
}

#[tokio::test]
async fn unread_http_download_releases_its_slot_without_disconnecting() {
    let fixture = Fixture::open().await;
    let bytes = vec![b'x'; 256 * 1024];
    let file = fixture.activate(&bytes).await;
    let router = fixture.router();
    let uri = format!("/api/files/{file}/content");
    let mut permits = Vec::new();
    while let Some(permit) = fixture.storage.try_acquire_download() {
        permits.push(permit);
    }
    drop(permits.pop());
    let stalled = call(
        &router,
        request("GET", &uri, Some(&fixture.login), false, None),
    )
    .await;
    assert_eq!(stalled.status(), StatusCode::OK);
    assert_eq!(
        status_of(
            &router,
            request("GET", &uri, Some(&fixture.login), false, None)
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // Keep the HTTP response alive without polling it, just as socket backpressure does.
    tokio::time::timeout(Duration::from_secs(33), async {
        loop {
            if let Some(permit) = fixture.storage.try_acquire_download() {
                drop(permit);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stalled delivery must release capacity");
    let response = call(
        &router,
        request("GET", &uri, Some(&fixture.login), false, None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_of(response).await, bytes);
    assert!(
        stalled.into_body().collect().await.is_err(),
        "a timed-out transfer must not look complete"
    );
}

#[tokio::test]
async fn uploads_and_confirmations_are_refused_as_busy_while_every_storage_slot_is_held() {
    let fixture = Fixture::open().await;
    let reservation = fixture
        .reserve(
            b"queued",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let slots = take_storage_slots(&fixture.storage);

    // A busy node refuses at once; nothing waits behind the requests holding the slots.
    let put = fixture.put(&reservation, b"queued", "text/plain");
    let refused = tokio::time::timeout(Duration::from_secs(5), put).await;
    assert_eq!(
        refused.expect("a busy upload is refused, not queued"),
        Err(LocalUploadError::StorageUnavailable)
    );
    let confirm = fixture.confirm(reservation.file_id);
    let refused = tokio::time::timeout(Duration::from_secs(5), confirm).await;
    assert_eq!(
        refused.expect("a busy confirmation is refused, not queued"),
        Err(ConfirmationError::StorageUnavailable)
    );
    drop(slots);

    // The same pending upload completes once a slot is free.
    fixture
        .put(&reservation, b"queued", "text/plain")
        .await
        .unwrap();
    fixture.confirm(reservation.file_id).await.unwrap();
    assert_eq!(
        fixture
            .bytes(reservation.file_id, Some(fixture.owner))
            .await
            .unwrap(),
        b"queued"
    );
}

#[tokio::test]
async fn a_stalled_upload_is_cut_off_when_its_window_ends() {
    let fixture = Fixture::open().await;
    let reservation = fixture
        .reserve(
            b"stalled!",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    // Two seconds of the window are left.
    sqlx::query("UPDATE files SET upload_expires_at=?1 WHERE id=?2")
        .bind(timestamp(chrono::Utc::now() + chrono::Duration::seconds(2)))
        .bind(reservation.file_id)
        .execute(&fixture.raw)
        .await
        .unwrap();

    // One byte arrives, then the sender goes quiet without closing the stream.
    let slots = free_slots(&fixture.storage);
    let (reader, mut writer) = tokio::io::duplex(64);
    writer.write_all(b"s").await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        files::put_local_upload(
            &fixture.pool,
            &fixture.storage,
            fixture.owner,
            reservation.file_id,
            "text/plain",
            reader,
        ),
    )
    .await
    .expect("a stalled upload must not outlast its window");
    assert_eq!(result.unwrap_err(), LocalUploadError::Rejected);
    assert!(fixture.stored_names().is_empty());
    drop(writer);
    assert_eq!(
        free_slots(&fixture.storage),
        slots,
        "the upload must give its storage slot back"
    );
}

#[tokio::test]
async fn upload_route_needs_a_session_a_csrf_token_and_the_owner() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let pending = fixture
        .reserve(
            b"endpoint",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let owner = &fixture.login;
    let stranger = &fixture.stranger_login;
    let url = pending.url.as_str();
    let send = |request| status_of(&router, request);

    assert_eq!(
        send(put_content(
            url,
            None,
            Some(&owner.csrf),
            Body::from("endpoint")
        ))
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(put_content(url, Some(owner), None, Body::from("endpoint"))).await,
        StatusCode::FORBIDDEN,
        "no CSRF token"
    );
    assert_eq!(
        send(put_content(
            url,
            Some(owner),
            Some(&"c".repeat(64)),
            Body::from("endpoint")
        ))
        .await,
        StatusCode::FORBIDDEN,
        "wrong CSRF token"
    );
    assert_eq!(
        send(put_content(
            url,
            Some(owner),
            Some(&stranger.csrf),
            Body::from("endpoint")
        ))
        .await,
        StatusCode::FORBIDDEN,
        "another session's CSRF token"
    );
    let mut cross_site = put_content(url, Some(owner), Some(&owner.csrf), Body::from("endpoint"));
    cross_site
        .headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(send(cross_site).await, StatusCode::FORBIDDEN);
    assert_eq!(
        send(put_content(
            url,
            Some(stranger),
            Some(&stranger.csrf),
            Body::from("endpoint")
        ))
        .await,
        StatusCode::NOT_FOUND,
        "someone else's upload does not exist for you"
    );

    let mut untyped = put_content(url, Some(owner), Some(&owner.csrf), Body::from("endpoint"));
    untyped.headers_mut().remove(header::CONTENT_TYPE);
    assert_eq!(send(untyped).await, StatusCode::BAD_REQUEST);
    let mut huge = put_content(url, Some(owner), Some(&owner.csrf), Body::from("endpoint"));
    huge.headers_mut()
        .insert(header::CONTENT_LENGTH, "104857601".parse().unwrap());
    assert_eq!(send(huge).await, StatusCode::PAYLOAD_TOO_LARGE);

    // The bytes actually received are what is bounded, whatever the headers say.
    let mut over_bound = put_content(url, Some(owner), Some(&owner.csrf), Body::from("endpoints"));
    over_bound
        .headers_mut()
        .insert(header::CONTENT_LENGTH, "8".parse().unwrap());
    assert_eq!(send(over_bound).await, StatusCode::UNPROCESSABLE_ENTITY);
    let wrong = call(
        &router,
        put_content(url, Some(owner), Some(&owner.csrf), Body::from("endpoinX")),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(wrong).await["error"], "upload_rejected");
    assert!(fixture.stored_names().is_empty());

    // Many empty frames before the data do not stop the upload from completing.
    let frames = std::iter::repeat_with(|| Ok::<_, std::io::Error>(bytes::Bytes::new()))
        .take(200)
        .chain(std::iter::once(Ok(bytes::Bytes::from_static(b"endpoint"))));
    assert_eq!(
        send(put_content(
            url,
            Some(owner),
            Some(&owner.csrf),
            Body::from_stream(futures_util::stream::iter(frames))
        ))
        .await,
        StatusCode::NO_CONTENT
    );
    // Other bytes cannot replace the accepted ones.
    assert_eq!(
        send(put_content(
            url,
            Some(owner),
            Some(&owner.csrf),
            Body::from("endpoinX")
        ))
        .await,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    fixture.confirm(pending.file_id).await.unwrap();
    assert_eq!(
        send(put_content(
            url,
            Some(owner),
            Some(&owner.csrf),
            Body::from("endpoint")
        ))
        .await,
        StatusCode::CONFLICT,
        "a confirmed upload takes no more bytes"
    );
    assert_eq!(
        fixture
            .bytes(pending.file_id, Some(fixture.owner))
            .await
            .unwrap(),
        b"endpoint"
    );
}

#[tokio::test]
async fn a_form_content_type_does_not_limit_a_streamed_upload() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let bytes = vec![b'x'; 5 * 1024 * 1024];
    let pending = fixture
        .reserve(
            &bytes,
            "application/x-www-form-urlencoded",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let mut upload = put_content(
        pending.url.as_str(),
        Some(&fixture.login),
        Some(&fixture.login.csrf),
        Body::from(bytes.clone()),
    );
    upload.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/x-www-form-urlencoded".parse().unwrap(),
    );
    assert_eq!(status_of(&router, upload).await, StatusCode::NO_CONTENT);
    fixture.confirm(pending.file_id).await.unwrap();
    assert_eq!(
        fixture
            .bytes(pending.file_id, Some(fixture.owner))
            .await
            .unwrap(),
        bytes
    );
}

#[tokio::test]
async fn json_api_reserves_uploads_confirms_downloads_and_deletes() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let owner = &fixture.login;
    let stranger = &fixture.stranger_login;
    let empty = call(
        &router,
        request("GET", "/api/files/stats", Some(owner), false, None),
    )
    .await;
    assert_eq!(empty.status(), StatusCode::OK);
    let empty = json_of(empty).await;
    assert_eq!(empty["total_files"], 0);
    assert_eq!(empty["total_bytes"], 0);
    assert_eq!(empty["by_reason"]["other"], 0);
    assert_eq!(empty["last_24h"]["uploads"], 0);
    assert!(files_db::get_user_file_stats(&fixture.pool, i64::MAX)
        .await
        .unwrap()
        .is_none());
    let content = b"hello from the api";
    let declaration = json!({
        "filename": "notes.txt",
        "content_type": "text/plain",
        "file_size": content.len(),
        "reason": "other",
        "visibility": "owner",
        "sha256_hex": digest(content),
    });

    // Reserving needs a session and a CSRF token.
    assert_eq!(
        status_of(
            &router,
            request("POST", "/api/files", None, false, Some(&declaration))
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(
            &router,
            request("POST", "/api/files", Some(owner), false, Some(&declaration))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let created = call(
        &router,
        request("POST", "/api/files", Some(owner), true, Some(&declaration)),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let created = json_of(created).await;
    let id = created["file_id"].as_str().unwrap().to_owned();
    assert_eq!(created["upload_url"], format!("/api/files/{id}/content"));
    assert_eq!(created["method"], "PUT");
    assert_eq!(created["capability"], "authenticated_local_put");
    assert_eq!(created["csrf_required"], true);
    assert_eq!(created["expires_in"], 900);
    assert!(created["s3_key"].is_null());
    assert_eq!(created["headers"]["content-type"], "text/plain");
    assert_eq!(
        created["headers"]["content-length"],
        content.len().to_string()
    );

    // A declaration the policy refuses gets a stable error.
    let mut refused = declaration.clone();
    refused["visibility"] = json!("public");
    let response = call(
        &router,
        request("POST", "/api/files", Some(owner), true, Some(&refused)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_of(response).await["error"], "invalid_upload");

    // Send the bytes, then confirm. Confirming needs the owner and a token too.
    let upload_url = created["upload_url"].as_str().unwrap();
    assert_eq!(
        status_of(
            &router,
            put_content(
                upload_url,
                Some(owner),
                Some(&owner.csrf),
                Body::from(content.to_vec())
            )
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let confirm = json!({ "file_id": id });
    assert_eq!(
        status_of(
            &router,
            request(
                "POST",
                "/api/files/confirm",
                Some(owner),
                false,
                Some(&confirm)
            )
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status_of(
            &router,
            request(
                "POST",
                "/api/files/confirm",
                Some(stranger),
                true,
                Some(&confirm)
            )
        )
        .await,
        StatusCode::NOT_FOUND
    );
    let confirmed = call(
        &router,
        request(
            "POST",
            "/api/files/confirm",
            Some(owner),
            true,
            Some(&confirm),
        ),
    )
    .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    assert_eq!(json_of(confirmed).await, json!({ "success": true }));
    let again = call(
        &router,
        request(
            "POST",
            "/api/files/confirm",
            Some(owner),
            true,
            Some(&confirm),
        ),
    )
    .await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(json_of(again).await["error"], "not_pending");

    // The owner's download link and statistics.
    let link = call(
        &router,
        request(
            "GET",
            &format!("/api/files/{id}/dl"),
            Some(owner),
            false,
            None,
        ),
    )
    .await;
    assert_eq!(link.status(), StatusCode::OK);
    assert_eq!(link.headers()[header::CACHE_CONTROL], "no-store, private");
    let link = json_of(link).await;
    assert_eq!(link["download_url"], format!("/api/files/{id}/content"));
    assert_eq!(link["capability"], "authorized_application_get");
    assert_eq!(link["filename"], "notes.txt");
    assert_eq!(link["content_type"], "text/plain");
    assert_eq!(link["file_size"], content.len());
    assert_eq!(link["expires_in"], 0);
    assert_eq!(
        status_of(
            &router,
            request(
                "GET",
                &format!("/api/files/{id}/dl"),
                Some(stranger),
                false,
                None
            )
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(
            &router,
            request("GET", &format!("/api/files/{id}/dl"), None, false, None)
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(
            &router,
            request("GET", "/api/files/not-a-file/dl", Some(owner), false, None)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let stats = call(
        &router,
        request("GET", "/api/files/stats", Some(owner), false, None),
    )
    .await;
    assert_eq!(stats.status(), StatusCode::OK);
    let stats = json_of(stats).await;
    assert_eq!(stats["total_files"], 1);
    assert_eq!(stats["total_bytes"], content.len());
    assert_eq!(stats["by_reason"]["other"], 1);
    assert_eq!(stats["last_24h"]["uploads"], 1);
    assert_eq!(stats["last_24h"]["downloads"], 1);
    assert_eq!(
        status_of(
            &router,
            request("GET", "/api/files/stats", None, false, None)
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // Deleting needs the owner and a token, and removes the stored bytes.
    let delete = format!("/api/files/{id}");
    assert_eq!(
        status_of(
            &router,
            request("DELETE", &delete, Some(owner), false, None)
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status_of(
            &router,
            request("DELETE", &delete, Some(stranger), true, None)
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(
            &router,
            request("DELETE", "/api/files/not-a-file", Some(owner), true, None)
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.stored_names(), vec![id.clone()]);
    assert_eq!(
        status_of(&router, request("DELETE", &delete, Some(owner), true, None)).await,
        StatusCode::NO_CONTENT
    );
    assert!(fixture.stored_names().is_empty());
    let stats = call(
        &router,
        request("GET", "/api/files/stats", Some(owner), false, None),
    )
    .await;
    assert_eq!(stats.status(), StatusCode::OK);
    let stats = json_of(stats).await;
    assert_eq!(stats["total_files"], 0);
    assert_eq!(stats["total_bytes"], 0);
    assert_eq!(
        stats["last_24h"]["uploads"], 1,
        "deletion does not refund quota"
    );
    assert_eq!(
        status_of(&router, request("DELETE", &delete, Some(owner), true, None)).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_json_api_enforces_upload_quotas() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    sqlx::query("UPDATE file_reason_limits SET max_uploads_per_hour=1 WHERE reason='other'")
        .execute(&fixture.raw)
        .await
        .unwrap();
    let declaration = json!({
        "filename": "notes.txt",
        "content_type": "text/plain",
        "file_size": 1,
        "reason": "other",
        "visibility": "owner",
        "sha256_hex": digest(b"x"),
    });
    for expected in [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS] {
        let response = call(
            &router,
            request(
                "POST",
                "/api/files",
                Some(&fixture.login),
                true,
                Some(&declaration),
            ),
        )
        .await;
        assert_eq!(response.status(), expected);
        if expected == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(json_of(response).await["error"], "rate_limited");
        }
    }
}

#[tokio::test]
async fn uploaded_files_download_as_attachments_except_public_avatar_images() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let page = b"<script>alert(1)</script>";
    let reservation = files::reserve_and_sign_upload(
        &fixture.pool,
        &fixture.storage,
        fixture.owner,
        UploadDeclaration {
            filename: "../../evil\"name\r\n.html",
            content_type: "text/html",
            file_size: page.len() as i64,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(page),
        },
    )
    .await
    .unwrap();
    files::put_local_upload(
        &fixture.pool,
        &fixture.storage,
        fixture.owner,
        reservation.file_id,
        "text/html",
        Cursor::new(page),
    )
    .await
    .unwrap();
    fixture.confirm(reservation.file_id).await.unwrap();

    // An uploaded web page is never rendered by the site.
    let uri = format!("/api/files/{}/content", reservation.file_id);
    let response = call(
        &router,
        request("GET", &uri, Some(&fixture.login), false, None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename*=UTF-8''evil%22name__.html"
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        "default-src 'none'; sandbox"
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store, private");
    assert_eq!(headers[header::CONTENT_LENGTH], page.len().to_string());
    assert_eq!(body_of(response).await, page);
    for login in [None, Some(&fixture.stranger_login)] {
        let hidden = call(&router, request("GET", &uri, login, false, None)).await;
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_of(hidden).await["error"], "not_found");
    }

    // A public avatar is the one thing shown inline, to anyone.
    let avatar = fixture
        .store(PNG, "image/png", FileReason::Avatar, FileVisibility::Public)
        .await;
    let public = call(
        &router,
        request(
            "GET",
            &format!("/api/files/{avatar}/content"),
            None,
            false,
            None,
        ),
    )
    .await;
    assert_eq!(public.status(), StatusCode::OK);
    assert_eq!(public.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(public.headers()[header::CONTENT_DISPOSITION], "inline");
    assert_eq!(public.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(body_of(public).await, PNG);
    // Pages link avatars through the /download route, which behaves the same.
    let linked = call(
        &router,
        request(
            "GET",
            &format!("/api/files/{avatar}/download"),
            None,
            false,
            None,
        ),
    )
    .await;
    assert_eq!(linked.status(), StatusCode::OK);
    assert_eq!(linked.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(linked.headers()[header::CONTENT_DISPOSITION], "inline");
    // The JSON form of the public link works without a session too.
    let link = call(
        &router,
        request(
            "GET",
            &format!("/api/files/public/{avatar}"),
            None,
            false,
            None,
        ),
    )
    .await;
    assert_eq!(link.status(), StatusCode::OK);
    assert_eq!(
        json_of(link).await["download_url"],
        format!("/api/files/{avatar}/content")
    );
    assert_eq!(
        status_of(
            &router,
            request(
                "GET",
                &format!("/api/files/public/{}", reservation.file_id),
                None,
                false,
                None
            )
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // On a private node nothing is public to people who are not signed in.
    sqlx::query("UPDATE node_configuration SET node_mode='private' WHERE id=1")
        .execute(&fixture.raw)
        .await
        .unwrap();
    let uri = format!("/api/files/{avatar}/content");
    assert_eq!(
        status_of(&router, request("GET", &uri, None, false, None)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(
            &router,
            request("GET", &uri, Some(&fixture.stranger_login), false, None)
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn storage_overview_is_for_admins_and_shows_no_paths_or_secrets() {
    let fixture = Fixture::open().await;
    let admin: i64 = sqlx::query_scalar(
        "INSERT INTO users(username,is_admin) VALUES ('local_admin',1) RETURNING id",
    )
    .fetch_one(&fixture.raw)
    .await
    .unwrap();
    let admin_login = login(&fixture.pool, admin, 'e', 'f').await;
    let router = fixture.router();
    let get = |login: Option<&Login>| {
        let mut builder = Request::builder()
            .uri("/admin/storage")
            .header(header::ACCEPT, "application/vnd.codeyard.page+json");
        if let Some(login) = login {
            builder = builder.header(header::COOKIE, format!("session_id={}", login.session));
        }
        builder.body(Body::empty()).unwrap()
    };
    for denied in [None, Some(&fixture.login)] {
        assert_eq!(status_of(&router, get(denied)).await, StatusCode::FORBIDDEN);
    }
    let response = call(&router, get(Some(&admin_login))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_of(response).await).unwrap();
    let envelope: PageEnvelope = serde_json::from_str(&text).unwrap();
    assert_eq!(
        envelope.page,
        Page::Storage(storage_view::Page {
            database: "SQLite".into(),
            object_storage: "Local directory".into(),
            location: None,
        })
    );
    assert!(!text.contains(fixture.temporary.path().to_str().unwrap()));
}

#[tokio::test]
async fn upload_collection_keeps_both_nested_root_spellings_and_csrf_checks() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let declaration = json!({
        "filename": "owned.txt",
        "content_type": "text/plain",
        "file_size": 5,
        "reason": "other",
        "visibility": "owner",
        "sha256_hex": digest(b"owned"),
    });
    for uri in ["/api/files", "/api/files/"] {
        let denied = call(
            &router,
            request("POST", uri, Some(&fixture.login), false, Some(&declaration)),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let response = call(
            &router,
            request("POST", uri, Some(&fixture.login), true, Some(&declaration)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let reservation = json_of(response).await;
        assert_eq!(reservation["capability"], "authenticated_local_put");
        assert_eq!(reservation["csrf_required"], true);
        assert_eq!(reservation["method"], "PUT");
    }
}

#[tokio::test]
async fn local_storage_io_failures_are_retryable_and_public_errors_are_sanitized() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let reservation = fixture
        .reserve(
            b"retry",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let parent = fixture
        .object_path(reservation.file_id)
        .parent()
        .unwrap()
        .to_owned();
    // A file in place of the upload directory causes real filesystem IO failure.
    std::fs::write(&parent, b"not a directory").unwrap();
    assert_eq!(
        fixture
            .put(&reservation, b"retry", "text/plain")
            .await
            .unwrap_err(),
        LocalUploadError::StorageUnavailable
    );
    assert_eq!(
        fixture.confirm(reservation.file_id).await.unwrap_err(),
        ConfirmationError::StorageUnavailable
    );
    let response = call(
        &router,
        request(
            "POST",
            "/api/files/confirm",
            Some(&fixture.login),
            true,
            Some(&json!({"file_id": reservation.file_id})),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error = json_of(response).await;
    assert_eq!(error["error"], "storage_unavailable");
    assert_eq!(
        error["message"],
        "Upload storage is unavailable; retry this pending upload"
    );
    assert!(!error
        .to_string()
        .contains(fixture.temporary.path().to_str().unwrap()));
    assert!(
        !files_db::get_file_by_id(&fixture.pool, reservation.file_id)
            .await
            .unwrap()
            .unwrap()
            .is_active
    );

    std::fs::remove_file(parent).unwrap();
    fixture
        .put(&reservation, b"retry", "text/plain")
        .await
        .unwrap();
    fixture.confirm(reservation.file_id).await.unwrap();
}

#[tokio::test]
async fn failed_metadata_deletion_keeps_bytes_and_is_sanitized() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let id = fixture.activate(b"deleted first").await;
    sqlx::query("CREATE TRIGGER refuse_file_delete BEFORE DELETE ON files BEGIN SELECT RAISE(ABORT, 'private database diagnostic'); END")
        .execute(&fixture.raw).await.unwrap();
    assert_eq!(
        files::delete_owned_file(&fixture.pool, &fixture.storage, fixture.owner, id)
            .await
            .unwrap_err(),
        DeleteError::MetadataUnavailable
    );
    assert!(fixture.object_path(id).exists());
    assert!(files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .is_some());
    let response = call(
        &router,
        request(
            "DELETE",
            &format!("/api/files/{id}"),
            Some(&fixture.login),
            true,
            None,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error = json_of(response).await;
    assert_eq!(error["error"], "database_unavailable");
    assert_eq!(error["message"], "File metadata is unavailable");
    assert!(!error.to_string().contains("private database diagnostic"));
    sqlx::query("DROP TRIGGER refuse_file_delete")
        .execute(&fixture.raw)
        .await
        .unwrap();
    files::delete_owned_file(&fixture.pool, &fixture.storage, fixture.owner, id)
        .await
        .unwrap();
    assert!(files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn expiry_failure_keeps_the_row_and_object_for_retry() {
    let fixture = Fixture::open().await;
    let reservation = fixture
        .reserve(
            b"stale",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    fixture
        .put(&reservation, b"stale", "text/plain")
        .await
        .unwrap();
    fixture.expire(reservation.file_id).await;
    sqlx::query("CREATE TRIGGER refuse_expiry_delete BEFORE DELETE ON files BEGIN SELECT RAISE(ABORT, 'private expiry diagnostic'); END")
        .execute(&fixture.raw).await.unwrap();
    files::remove_expired_uploads(&fixture.pool, &fixture.storage)
        .await
        .unwrap_err();
    assert!(fixture.object_path(reservation.file_id).exists());
    assert!(files_db::get_file_by_id(&fixture.pool, reservation.file_id)
        .await
        .unwrap()
        .is_some());
    sqlx::query("DROP TRIGGER refuse_expiry_delete")
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(
        files::remove_expired_uploads(&fixture.pool, &fixture.storage)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn failed_object_removal_is_retried_from_persisted_work() {
    let fixture = Fixture::open().await;
    let router = fixture.router();
    let id = fixture.activate(b"kept until its bytes are gone").await;
    let path = fixture.object_path(id);
    // A directory where the stored file was: removing it as a file fails.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    files::delete_owned_file(&fixture.pool, &fixture.storage, fixture.owner, id)
        .await
        .unwrap();
    // Metadata is private immediately even though object cleanup must retry.
    assert!(files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .is_none());
    let uri = format!("/api/files/{id}");
    let response = call(
        &router,
        request("DELETE", &uri, Some(&fixture.login), true, None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .is_none());
    // A stranger still cannot tell the file existed.
    assert_eq!(
        status_of(
            &router,
            request("DELETE", &uri, Some(&fixture.stranger_login), true, None)
        )
        .await,
        StatusCode::NOT_FOUND
    );

    // Once the capability deadline has passed and storage works again, the
    // ordinary bounded sweep finishes the persisted cleanup.
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, b"bytes").unwrap();
    sqlx::query("UPDATE pending_object_deletions SET delete_after=?1")
        .bind(timestamp(chrono::Utc::now() - chrono::Duration::seconds(1)))
        .execute(&fixture.raw)
        .await
        .unwrap();
    files::remove_expired_uploads(&fixture.pool, &fixture.storage)
        .await
        .unwrap();
    assert!(!path.exists());
    assert!(files_db::get_file_by_id(&fixture.pool, id)
        .await
        .unwrap()
        .is_none());
}

/// How many queued removals of `key` the sweep would run within the next minute.
async fn due_soon(fixture: &Fixture, key: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM pending_object_deletions WHERE object_key=?1 AND delete_after <= strftime('%Y-%m-%dT%H:%M:%f000Z','now','+60 seconds')")
        .bind(key)
        .fetch_one(&fixture.raw)
        .await
        .unwrap()
}

/// How many removals of `key` are queued at all.
async fn queued_removals(fixture: &Fixture, key: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM pending_object_deletions WHERE object_key=?1")
        .bind(key)
        .fetch_one(&fixture.raw)
        .await
        .unwrap()
}

#[tokio::test]
async fn bytes_that_land_after_their_upload_deadline_stay_queued_for_the_grace_period() {
    let fixture = Fixture::open().await;
    let stale = fixture
        .reserve(
            b"late",
            "text/plain",
            FileReason::Other,
            FileVisibility::Owner,
        )
        .await;
    let key = format!("uploads/{}", stale.file_id);
    // The window and the PUT deadline both pass with no bytes.
    sqlx::query("UPDATE files SET upload_expires_at=?1,put_expires_at=?1 WHERE id=?2")
        .bind(timestamp(chrono::Utc::now() - chrono::Duration::seconds(1)))
        .bind(stale.file_id)
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(
        files::remove_expired_uploads(&fixture.pool, &fixture.storage)
            .await
            .unwrap(),
        1
    );
    assert!(files_db::get_file_by_id(&fixture.pool, stale.file_id)
        .await
        .unwrap()
        .is_none());

    // A PUT accepted before the deadline lands after the record is gone.
    std::fs::create_dir_all(fixture.object_path(stale.file_id).parent().unwrap()).unwrap();
    std::fs::write(fixture.object_path(stale.file_id), b"late").unwrap();
    assert_eq!(
        files::remove_expired_uploads(&fixture.pool, &fixture.storage)
            .await
            .unwrap(),
        0
    );
    assert!(
        fixture.object_path(stale.file_id).exists(),
        "late bytes wait out the grace period"
    );
    assert_eq!(due_soon(&fixture, &key).await, 0);

    // Once the queued time has come, the next sweep removes them.
    sqlx::query("UPDATE pending_object_deletions SET delete_after=?1 WHERE object_key=?2")
        .bind(timestamp(chrono::Utc::now() - chrono::Duration::seconds(1)))
        .bind(&key)
        .execute(&fixture.raw)
        .await
        .unwrap();
    files::remove_expired_uploads(&fixture.pool, &fixture.storage)
        .await
        .unwrap();
    assert!(!fixture.object_path(stale.file_id).exists());
}

#[tokio::test]
async fn keys_that_cannot_be_removed_do_not_hold_back_the_queue() {
    let fixture = Fixture::open().await;
    let now = chrono::Utc::now();
    let uploads = fixture.state_path().join("objects/uploads");
    std::fs::create_dir_all(&uploads).unwrap();
    // A directory cannot be removed as a file, so each of these keys fails. They
    // are due before the removable key, so a batch would take them first.
    for offset in 0..100i64 {
        let id = Uuid::new_v4();
        std::fs::create_dir(uploads.join(id.to_string())).unwrap();
        sqlx::query("INSERT INTO pending_object_deletions(object_key,delete_after) VALUES(?1,?2)")
            .bind(format!("uploads/{id}"))
            .bind(timestamp(
                now - chrono::Duration::hours(2) + chrono::Duration::seconds(offset),
            ))
            .execute(&fixture.raw)
            .await
            .unwrap();
    }
    let removable = Uuid::new_v4();
    std::fs::write(fixture.object_path(removable), b"removable").unwrap();
    sqlx::query("INSERT INTO pending_object_deletions(object_key,delete_after) VALUES(?1,?2)")
        .bind(format!("uploads/{removable}"))
        .bind(timestamp(now - chrono::Duration::minutes(1)))
        .execute(&fixture.raw)
        .await
        .unwrap();

    // The first sweep works through a full batch of failing keys.
    files::remove_expired_uploads(&fixture.pool, &fixture.storage)
        .await
        .unwrap();
    // The failing keys wait before their next attempt, so the next sweep reaches
    // the removable key.
    files::remove_expired_uploads(&fixture.pool, &fixture.storage)
        .await
        .unwrap();
    assert!(!fixture.object_path(removable).exists());
}

#[tokio::test]
async fn object_removal_is_refused_while_every_storage_slot_is_busy() {
    let fixture = Fixture::open().await;
    let id = fixture.activate(b"busy delete").await;
    let key = format!("uploads/{id}");
    let slots = take_storage_slots(&fixture.storage);
    assert!(
        fixture.storage.delete(&key).await.is_err(),
        "a delete takes a slot or is refused"
    );
    // The metadata goes at once; the bytes stay until a sweep can take a slot.
    files::delete_owned_file(&fixture.pool, &fixture.storage, fixture.owner, id)
        .await
        .unwrap();
    assert!(fixture.object_path(id).exists());
    assert_eq!(
        queued_removals(&fixture, &key).await,
        1,
        "the bytes stay queued until a sweep can take them"
    );
    drop(slots);

    sqlx::query("UPDATE pending_object_deletions SET delete_after=?1 WHERE object_key=?2")
        .bind(timestamp(chrono::Utc::now() - chrono::Duration::seconds(1)))
        .bind(&key)
        .execute(&fixture.raw)
        .await
        .unwrap();
    files::remove_expired_uploads(&fixture.pool, &fixture.storage)
        .await
        .unwrap();
    assert!(!fixture.object_path(id).exists());
}

#[tokio::test]
async fn a_replaced_picture_is_removed_with_the_change_or_not_at_all() {
    let fixture = Fixture::open().await;
    let old = fixture
        .store(PNG, "image/png", FileReason::Avatar, FileVisibility::Public)
        .await;
    let new = fixture
        .store(PNG, "image/png", FileReason::Avatar, FileVisibility::Public)
        .await;
    assert_eq!(
        db::users::accounts::set_user_avatar(&fixture.pool, fixture.owner, Some(old))
            .await
            .unwrap(),
        db::users::accounts::UserAvatarOutcome::Applied(None)
    );

    // If the replaced picture cannot be removed, the change is refused as a whole.
    sqlx::query("CREATE TRIGGER refuse_picture_delete BEFORE DELETE ON files BEGIN SELECT RAISE(ABORT, 'private database diagnostic'); END")
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(
        db::users::accounts::set_user_avatar(&fixture.pool, fixture.owner, Some(new))
            .await
            .is_err()
    );
    let pointer: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT avatar_file_id FROM users WHERE id=?1")
            .bind(fixture.owner)
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(pointer, Some(old));
    sqlx::query("DROP TRIGGER refuse_picture_delete")
        .execute(&fixture.raw)
        .await
        .unwrap();

    // Otherwise the old row goes with the change, and its bytes are queued.
    assert_eq!(
        db::users::accounts::set_user_avatar(&fixture.pool, fixture.owner, Some(new))
            .await
            .unwrap(),
        db::users::accounts::UserAvatarOutcome::Applied(Some(old))
    );
    assert!(files_db::get_file_by_id(&fixture.pool, old)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        queued_removals(&fixture, &format!("uploads/{old}")).await,
        1
    );
}

#[tokio::test]
async fn a_banned_account_keeps_its_picture() {
    let fixture = Fixture::open().await;
    let old = fixture
        .store(PNG, "image/png", FileReason::Avatar, FileVisibility::Public)
        .await;
    let new = fixture
        .store(PNG, "image/png", FileReason::Avatar, FileVisibility::Public)
        .await;
    // The request was authenticated before the ban, so it still holds the account as it was.
    let user = db::users::accounts::get_user_by_id(&fixture.pool, fixture.owner)
        .await
        .unwrap()
        .unwrap();
    avatars::set_user_avatar(&fixture.pool, &user, old)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=?1")
        .bind(fixture.owner)
        .execute(&fixture.raw)
        .await
        .unwrap();

    let changed = avatars::set_user_avatar(&fixture.pool, &user, new).await;
    assert_eq!(changed, Err(AvatarError::Denied));
    let cleared = avatars::remove_user_avatar(&fixture.pool, &user).await;
    assert_eq!(cleared, Err(AvatarError::Denied));
    let pointer: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT avatar_file_id FROM users WHERE id=?1")
            .bind(fixture.owner)
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(pointer, Some(old));
    assert!(files_db::get_file_by_id(&fixture.pool, old)
        .await
        .unwrap()
        .is_some());
}
