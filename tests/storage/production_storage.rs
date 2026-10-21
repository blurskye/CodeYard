//! Upload records and quotas on a real PostgreSQL schema, and the S3 side of
//! uploads: signing, confirming, downloading and deleting.
//!
//! The PostgreSQL tests need GITSERV_TEST_DATABASE_URL to name a disposable
//! database called `gitserv_test`; each test works in its own schema. A missing
//! database fails the test, it is never skipped.
//!
//! The tests that talk HTTP to S3 use a small in-memory stand-in on a loopback
//! port. It shows how the application behaves for each response, but it is not
//! S3: it checks no signatures. The `#[ignore]`d tests at the end run the same
//! flows against a real S3-compatible server, which must be a throwaway fixture:
//!
//! - GITSERV_STORAGE_TEST_ENDPOINT: loopback URL with a high port, e.g.
//!   http://127.0.0.1:11082
//! - GITSERV_STORAGE_TEST_PUBLIC_ENDPOINT: the address presigned URLs point at;
//!   the first of these tests needs it to differ from the endpoint above
//! - GITSERV_STORAGE_TEST_BUCKET: named gitserv-storage-test-<32 hex digits>
//! - GITSERV_STORAGE_TEST_ACCESS_KEY and GITSERV_STORAGE_TEST_SECRET_KEY
//!
//! The bucket must already exist and contain the object `_gitserv_fixture_owner`
//! with the body `production-storage-v1:<bucket>`. The tests never create,
//! list or delete buckets and only remove objects they created. They also need
//! the PostgreSQL variable above.
use crate::support::native_database as native;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use base64::Engine;
use codeyard::backend::{
    db::{
        self,
        files::{self, AdmissionError, ReserveUpload},
        models::{File, FileReason, FileVisibility},
    },
    files::{
        self as file_operations, ConfirmationError, DeleteError, DownloadCapability, DownloadError,
        DownloadTransport, LocalUploadError, UploadCapability, UploadDeclaration, UploadError,
    },
    identity::avatars::{self, AvatarError},
    storage::{
        s3::{self, S3Config},
        ObjectStore,
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use uuid::Uuid;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

struct Database {
    pool: db::DbPool,
    raw: sqlx::PgPool,
    user: i64,
    _schema: native::PostgresDatabase,
}

async fn database() -> Database {
    let schema = native::PostgresDatabase::new().await;
    let user =
        sqlx::query_scalar("INSERT INTO users(username) VALUES ('storage_owner') RETURNING id")
            .fetch_one(&schema.raw)
            .await
            .unwrap();
    Database {
        pool: schema.pool.clone(),
        raw: schema.raw.clone(),
        user,
        _schema: schema,
    }
}

/// Sets the size and count limits of the "other" upload reason.
async fn limits(db: &Database, bytes: i64, count: i32) {
    sqlx::query("UPDATE file_reason_limits SET max_file_size_bytes = $1, max_bytes_per_hour = $1, max_bytes_per_day = $1, max_uploads_per_hour = $2, max_uploads_per_day = $2 WHERE reason = 'other'")
        .bind(bytes).bind(count).execute(&db.raw).await.unwrap();
}

fn digest(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Records a pending 7-byte "fixture.txt" upload, as `reserve_and_sign_upload`
/// does before it signs anything.
async fn reserve(db: &Database, bytes: i64) -> Result<File, AdmissionError> {
    files::reserve_upload(
        &db.pool,
        ReserveUpload {
            user_id: db.user,
            filename: "fixture.txt",
            content_type: "text/plain",
            file_size: bytes,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(b"fixture"),
        },
    )
    .await
}

async fn reserve_avatar(db: &Database, visibility: FileVisibility) -> File {
    files::reserve_upload(
        &db.pool,
        ReserveUpload {
            user_id: db.user,
            filename: "me.png",
            content_type: "image/png",
            file_size: PNG.len() as i64,
            reason: FileReason::Avatar,
            visibility,
            sha256_hex: &digest(PNG),
        },
    )
    .await
    .unwrap()
}

async fn activate(db: &Database, id: Uuid) {
    assert!(files::activate_upload(&db.pool, id, db.user).await.unwrap());
}

/// Ends the upload window of a pending file.
async fn expire(db: &Database, id: Uuid) {
    sqlx::query("UPDATE files SET upload_expires_at = NOW() - INTERVAL '1 second' WHERE id = $1")
        .bind(id)
        .execute(&db.raw)
        .await
        .unwrap();
}

/// Inserts an active file directly, for tests of how files are read.
async fn insert_active_file(db: &Database, reason: &str, content_type: &str, size: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO files(id,user_id,filename,original_name,content_type,file_size,sha256_hex,object_key,reason,is_active) VALUES ($1,$2,'raw','raw',$3,$4,$5,$6,$7,true)")
        .bind(id)
        .bind(db.user)
        .bind(content_type)
        .bind(size)
        .bind(digest(b"raw"))
        .bind(format!("uploads/{id}"))
        .bind(reason)
        .execute(&db.raw)
        .await
        .unwrap();
    id
}

fn s3_config(endpoint: &str, bucket: &str) -> S3Config {
    S3Config {
        endpoint: endpoint.into(),
        public_endpoint: None,
        access_key: "fixture".into(),
        secret_key: "fixture".into(),
        bucket: bucket.into(),
        region: "us-east-1".into(),
    }
}

fn browser_s3(public_endpoint: &str) -> S3Config {
    let mut config = s3_config("http://storage.internal:9000", "browser-fixture");
    config.public_endpoint = Some(public_endpoint.into());
    config
}

#[test]
fn browser_s3_host_must_be_outside_the_session_cookie_scope() {
    for (endpoint, public_url, cookie_domain) in [
        (
            "http://forge.example.test:9000",
            "https://forge.example.test:443",
            None,
        ),
        (
            "https://objects.example.test:9443",
            "https://forge.example.test",
            Some(".example.test"),
        ),
        (
            "https://example.test",
            "https://forge.example.test",
            Some("example.test"),
        ),
        ("http://localhost:9000", "http://localhost:3000", None),
        (
            "http://objects.localhost:9000",
            "http://forge.localhost:3000",
            Some(".localhost"),
        ),
        ("http://127.0.0.1:9000", "http://127.0.0.1:3000", None),
        ("https://[::1]:9000", "http://[::1]:3000", None),
    ] {
        let error = browser_s3(endpoint)
            .validate_browser_cookie_scope(Some(public_url), cookie_domain)
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("session cookie scope"), "{message}");
        assert!(!message.contains(endpoint), "endpoint leaked: {message}");
    }
}

#[test]
fn browser_s3_allows_hosts_that_cannot_receive_the_session_cookie() {
    for (endpoint, public_url, cookie_domain) in [
        (
            "https://objects.example.test",
            "https://forge.example.test",
            None,
        ),
        (
            "https://objects.example.test",
            "https://forge.example.test",
            Some("forge.example.test"),
        ),
        (
            "https://objects.provider.test",
            "https://forge.example.test",
            Some(".example.test"),
        ),
        (
            "https://notexample.test",
            "https://forge.example.test",
            Some("example.test"),
        ),
        (
            "http://objects.localhost:9000",
            "http://localhost:3000",
            None,
        ),
        ("http://127.0.0.2:9000", "http://127.0.0.1:3000", None),
    ] {
        browser_s3(endpoint)
            .validate_browser_cookie_scope(Some(public_url), cookie_domain)
            .unwrap();
    }
}

#[test]
fn browser_s3_cookie_scope_settings_are_validated_without_echoing_values() {
    let config = browser_s3("https://objects.example.test");
    assert!(config
        .validate_browser_cookie_scope(None, None)
        .unwrap_err()
        .to_string()
        .contains("PUBLIC_URL"));
    let public_url = "https://operator:public-url-password@forge.example.test";
    let message = config
        .validate_browser_cookie_scope(Some(public_url), None)
        .unwrap_err()
        .to_string();
    assert!(message.contains("PUBLIC_URL"), "{message}");
    assert!(
        !message.contains(public_url),
        "public URL leaked: {message}"
    );
    assert!(!message.contains("public-url-password"), "{message}");
    for domain in [
        "https://secret-domain.example.test",
        "secret-domain.example.test:443",
        "..secret-domain.example.test",
    ] {
        let message = config
            .validate_browser_cookie_scope(Some("https://forge.example.test"), Some(domain))
            .unwrap_err()
            .to_string();
        assert!(message.contains("COOKIE_DOMAIN"), "{message}");
        assert!(!message.contains(domain), "cookie domain leaked: {message}");
    }
}

/// An S3 store that can sign URLs but has nothing to talk to: signing is a
/// local computation, so nothing listens at the endpoint.
async fn signing_store(bucket: &str) -> ObjectStore {
    let state = tempfile::tempdir().unwrap();
    ObjectStore::open(state.path(), Some(s3_config("http://127.0.0.1:9", bucket)))
        .await
        .unwrap()
}

#[derive(Clone, Default)]
struct S3State {
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    deleted: Arc<Mutex<Vec<String>>>,
    refuse_deletes: Arc<AtomicBool>,
    refuse_puts: Arc<AtomicBool>,
    refuse_reads: Arc<AtomicBool>,
    /// The headers of every PUT received, to check what the application signed.
    puts: Arc<Mutex<Vec<HeaderMap>>>,
}

/// Answers the requests the application makes of an uploaded object: HEAD, GET
/// (including a ranged read of the first bytes), DELETE, and PUT for bytes the
/// server uploads itself.
async fn fake_s3(
    State(state): State<S3State>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let path = uri.path().to_owned();
    let xml_error = |status: StatusCode, code: &str| -> Response {
        (
            status,
            [(header::CONTENT_TYPE, "application/xml")],
            format!("<Error><Code>{code}</Code></Error>"),
        )
            .into_response()
    };
    match method {
        Method::PUT => {
            state.puts.lock().unwrap().push(headers.clone());
            if state.refuse_puts.load(Ordering::SeqCst) {
                return xml_error(StatusCode::FORBIDDEN, "AccessDenied");
            }
            let mut objects = state.objects.lock().unwrap();
            // `If-None-Match: *` means "only if nothing is there yet".
            if headers
                .get("if-none-match")
                .is_some_and(|value| value == "*")
                && objects.contains_key(&path)
            {
                return xml_error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed");
            }
            objects.insert(path, body.to_vec());
            StatusCode::OK.into_response()
        }
        Method::DELETE => {
            state.deleted.lock().unwrap().push(path.clone());
            if state.refuse_deletes.load(Ordering::SeqCst) {
                return xml_error(StatusCode::FORBIDDEN, "AccessDenied");
            }
            state.objects.lock().unwrap().remove(&path);
            StatusCode::NO_CONTENT.into_response()
        }
        Method::HEAD | Method::GET => {
            if state.refuse_reads.load(Ordering::SeqCst) {
                return xml_error(StatusCode::FORBIDDEN, "AccessDenied");
            }
            let Some(object) = state.objects.lock().unwrap().get(&path).cloned() else {
                return xml_error(StatusCode::NOT_FOUND, "NoSuchKey");
            };
            if method == Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_LENGTH, object.len())
                    .body(Body::empty())
                    .unwrap();
            }
            let last = headers
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("bytes=0-"))
                .and_then(|value| value.parse::<usize>().ok());
            match last {
                Some(last) => (
                    StatusCode::PARTIAL_CONTENT,
                    object[..=last.min(object.len() - 1)].to_vec(),
                )
                    .into_response(),
                None => (StatusCode::OK, object).into_response(),
            }
        }
        _ => xml_error(StatusCode::NOT_IMPLEMENTED, "NotImplemented"),
    }
}

struct FakeS3 {
    state: S3State,
    config: S3Config,
    store: ObjectStore,
    task: tokio::task::JoinHandle<()>,
    _state_dir: tempfile::TempDir,
}

impl FakeS3 {
    async fn start(bucket: &str) -> Self {
        let state = S3State::default();
        let router = Router::new().fallback(fake_s3).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = s3_config(
            &format!("http://{}", listener.local_addr().unwrap()),
            bucket,
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let state_dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(state_dir.path(), Some(config.clone()))
            .await
            .unwrap();
        Self {
            state,
            config,
            store,
            task,
            _state_dir: state_dir,
        }
    }

    fn path(&self, key: &str) -> String {
        format!("/{}/{key}", self.config.bucket)
    }

    /// Puts an object in the bucket, as a client with a presigned URL would.
    fn put(&self, key: &str, bytes: &[u8]) {
        self.state
            .objects
            .lock()
            .unwrap()
            .insert(self.path(key), bytes.to_vec());
    }

    fn refuse_deletes(&self, refuse: bool) {
        self.state.refuse_deletes.store(refuse, Ordering::SeqCst);
    }

    fn refuse_puts(&self, refuse: bool) {
        self.state.refuse_puts.store(refuse, Ordering::SeqCst);
    }

    /// What is stored under `key`, if anything.
    fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.state
            .objects
            .lock()
            .unwrap()
            .get(&self.path(key))
            .cloned()
    }

    /// Paths of every DELETE received so far, refused or not.
    fn deleted(&self) -> Vec<String> {
        self.state.deleted.lock().unwrap().clone()
    }
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn query_of(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_reservations_sign_the_declared_object_and_charge_the_quota() {
    let db = database().await;
    limits(&db, 7, 1).await;
    let store = signing_store("owned-unit-fixture").await;
    let checksum = digest(b"fixture");
    let declaration = || UploadDeclaration {
        filename: "fixture.txt",
        content_type: "text/plain",
        file_size: 7,
        reason: FileReason::Other,
        visibility: FileVisibility::Owner,
        sha256_hex: &checksum,
    };
    let reservation =
        file_operations::reserve_and_sign_upload(&db.pool, &store, db.user, declaration())
            .await
            .unwrap();
    let file = files::get_file_by_id(&db.pool, reservation.file_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!file.is_active && file.upload_expires_at.is_some());
    assert_eq!(file.object_key, format!("uploads/{}", file.id));
    assert_eq!(reservation.object_key, file.object_key);
    assert_eq!(reservation.capability, UploadCapability::S3DirectPut);
    assert_eq!(reservation.method, "PUT");
    assert_eq!(reservation.expires_in, 900);

    // S3 enforces what is signed, so size, type and checksum must be signed.
    let url = url::Url::parse(&reservation.url).unwrap();
    assert!(url.path().ends_with(&file.object_key));
    let query = query_of(&reservation.url);
    let query = |name: &str| {
        query
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(query("x-amz-expires").as_deref(), Some("900"));
    let signed_headers = query("x-amz-signedheaders").unwrap();
    for name in ["content-length", "content-type", "if-none-match"] {
        assert!(
            signed_headers.split(';').any(|signed| signed == name),
            "{name} must be signed"
        );
    }
    assert_eq!(reservation.headers["content-length"], "7");
    assert_eq!(reservation.headers["content-type"], "text/plain");
    assert_eq!(reservation.headers["if-none-match"], "*");
    let expected = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"fixture"));
    assert!(
        query("x-amz-checksum-sha256").as_deref() == Some(expected.as_str())
            || reservation
                .headers
                .get("x-amz-checksum-sha256")
                .is_some_and(|value| *value == expected),
        "the SHA-256 of the file must be signed"
    );

    // The quota is charged when the URL is handed out.
    assert_eq!(
        file_operations::reserve_and_sign_upload(&db.pool, &store, db.user, declaration())
            .await
            .unwrap_err(),
        UploadError::Quota
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_downloads_are_signed_after_authorization_and_only_avatars_show_inline() {
    let db = database().await;
    let store = Arc::new(signing_store("owned-unit-fixture").await);
    let file = reserve(&db, 7).await.unwrap();
    assert_eq!(
        file_operations::download_asset(&db.pool, &store, Some(db.user), file.id)
            .await
            .unwrap_err(),
        DownloadError::NotFound,
        "a pending upload is not a download"
    );
    activate(&db, file.id).await;

    for actor in [None, Some(db.user + 1)] {
        assert_eq!(
            file_operations::download_asset(&db.pool, &store, actor, file.id)
                .await
                .unwrap_err(),
            DownloadError::NotFound
        );
        assert_eq!(
            file_operations::open_asset_download(&db.pool, &store, actor, file.id)
                .await
                .unwrap_err(),
            DownloadError::NotFound
        );
    }
    let asset = file_operations::download_asset(&db.pool, &store, Some(db.user), file.id)
        .await
        .unwrap();
    assert_eq!(asset.capability, DownloadCapability::S3PresignedGet);
    assert_eq!(asset.filename, "fixture.txt");
    assert_eq!(asset.content_type, "text/plain");
    assert_eq!(asset.file_size, 7);
    assert_eq!(asset.expires_in, 300);
    assert!(url::Url::parse(&asset.url)
        .unwrap()
        .path()
        .ends_with(&file.object_key));
    let query = query_of(&asset.url);
    assert_eq!(
        query["response-content-disposition"],
        "attachment; filename*=UTF-8''fixture.txt"
    );
    assert_eq!(query["response-content-type"], "application/octet-stream");

    // Only download_for_user counts as a download.
    let downloads = || async {
        files::get_user_file_stats(&db.pool, db.user)
            .await
            .unwrap()
            .unwrap()
            .downloads_24h
    };
    assert_eq!(downloads().await, 0);
    file_operations::download_for_user(&db.pool, &store, db.user, file.id)
        .await
        .unwrap();
    assert_eq!(downloads().await, 1);

    // Streaming routes redirect to the same kind of URL.
    let opened = file_operations::open_asset_download(&db.pool, &store, Some(db.user), file.id)
        .await
        .unwrap();
    assert_eq!(opened.inline_type, None);
    let DownloadTransport::RemoteUrl(remote) = opened.transport else {
        panic!("S3 downloads redirect to a signed URL")
    };
    assert_eq!(
        query_of(&remote)["response-content-disposition"],
        "attachment; filename*=UTF-8''fixture.txt"
    );

    // A public avatar image is the one thing displayed inline.
    let avatar = reserve_avatar(&db, FileVisibility::Public).await;
    activate(&db, avatar.id).await;
    for actor in [None, Some(db.user)] {
        let opened = file_operations::open_asset_download(&db.pool, &store, actor, avatar.id)
            .await
            .unwrap();
        assert_eq!(opened.inline_type.as_deref(), Some("image/png"));
        let DownloadTransport::RemoteUrl(remote) = opened.transport else {
            panic!("S3 downloads redirect to a signed URL")
        };
        let query = query_of(&remote);
        assert_eq!(query["response-content-disposition"], "inline");
        assert_eq!(query["response-content-type"], "image/png");
    }

    // The authorized HTTP redirect supplies the filename to the eventual S3 response.
    use tower::ServiceExt;
    sqlx::query("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE").execute(&db.raw).await.unwrap();
    sqlx::query("UPDATE files SET visibility='public',original_name=$2 WHERE id=$1")
        .bind(file.id)
        .bind("../../résumé \"final\"\r\n.txt")
        .execute(&db.raw)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = codeyard::backend::api::create_router(codeyard::backend::app::AppState::new(
        db.pool.clone(),
        root.path().to_owned(),
        store,
        None,
        codeyard::backend::ci::CiConfig::default(),
        None,
    ));
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/files/{}/download", file.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        query_of(response.headers()[header::LOCATION].to_str().unwrap())
            ["response-content-disposition"],
        "attachment; filename*=UTF-8''r%C3%A9sum%C3%A9%20%22final%22__.txt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_confirmation_needs_the_object_to_match_the_declaration() {
    let db = database().await;
    let s3 = FakeS3::start("owned-unit-fixture").await;
    let reservation = file_operations::reserve_and_sign_upload(
        &db.pool,
        &s3.store,
        db.user,
        UploadDeclaration {
            filename: "me.png",
            content_type: "image/png",
            file_size: PNG.len() as i64,
            reason: FileReason::Avatar,
            visibility: FileVisibility::Public,
            sha256_hex: &digest(PNG),
        },
    )
    .await
    .unwrap();
    let id = reservation.file_id;
    let key = reservation.object_key.as_str();
    let pending = || async {
        !files::get_file_by_id(&db.pool, id)
            .await
            .unwrap()
            .unwrap()
            .is_active
    };

    // The local upload route has nothing to do when the bytes go to S3.
    assert_eq!(
        file_operations::put_local_upload(
            &db.pool,
            &s3.store,
            db.user,
            id,
            "image/png",
            std::io::Cursor::new(PNG)
        )
        .await
        .unwrap_err(),
        LocalUploadError::Rejected
    );

    // Only the owner can confirm, and nothing has arrived yet.
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &s3.store, db.user + 1, id)
            .await
            .unwrap_err(),
        ConfirmationError::NotFound
    );
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &s3.store, db.user, id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    // An object of the wrong size, or of the right size but not a PNG, is refused.
    s3.put(key, b"\x89PNG\r\n\x1a\nextra");
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &s3.store, db.user, id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    s3.put(key, b"<html>!!");
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &s3.store, db.user, id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    assert!(
        pending().await,
        "failed confirmations leave the upload pending"
    );

    // Once the right bytes are there the same reservation can be confirmed, once.
    s3.put(key, PNG);
    file_operations::confirm_upload(&db.pool, &s3.store, db.user, id)
        .await
        .unwrap();
    assert!(!pending().await);
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &s3.store, db.user, id)
            .await
            .unwrap_err(),
        ConfirmationError::NotPending
    );
    assert!(files::get_downloadable_file(&db.pool, id, None)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_stores_bytes_the_server_holds_and_leaves_nothing_behind_on_failure() {
    let db = database().await;
    let s3 = FakeS3::start("owned-unit-fixture").await;
    let checksum = digest(PNG);
    let declaration = || UploadDeclaration {
        filename: "me.png",
        content_type: "image/png",
        file_size: PNG.len() as i64,
        reason: FileReason::Avatar,
        visibility: FileVisibility::Public,
        sha256_hex: &checksum,
    };
    let files_of_owner = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM files WHERE user_id = $1")
            .bind(db.user)
            .fetch_one(&db.raw)
            .await
            .unwrap()
    };

    // The bytes go to the bucket under the file's key, verified but still pending.
    let id =
        file_operations::store_upload(&db.pool, &s3.store, db.user, declaration(), PNG.to_vec())
            .await
            .unwrap();
    let file = files::get_file_by_id(&db.pool, id).await.unwrap().unwrap();
    assert!(!file.is_active);
    assert!(file.confirmed_at.is_some() && file.upload_expires_at.is_some());
    assert_eq!(s3.object(&file.object_key).as_deref(), Some(PNG));
    let put = s3.state.puts.lock().unwrap()[0].clone();
    assert_eq!(
        put.get("if-none-match").unwrap(),
        "*",
        "an object is never replaced"
    );
    assert_eq!(put.get(header::CONTENT_TYPE).unwrap(), "image/png");
    assert!(
        put.get("x-amz-checksum-sha256").is_some(),
        "S3 is asked to verify the checksum: {put:?}"
    );

    // Bytes that are not what was declared are refused before anything is sent.
    let other = b"\x89PNG\r\n\x1a\nnot the declared bytes".to_vec();
    assert_eq!(
        file_operations::store_upload(&db.pool, &s3.store, db.user, declaration(), other)
            .await
            .unwrap_err(),
        UploadError::StorageUnavailable
    );
    assert_eq!(s3.state.puts.lock().unwrap().len(), 1);
    // A bucket that refuses the upload leaves no pending file behind either.
    s3.refuse_puts(true);
    assert_eq!(
        file_operations::store_upload(&db.pool, &s3.store, db.user, declaration(), PNG.to_vec())
            .await
            .unwrap_err(),
        UploadError::StorageUnavailable
    );
    assert_eq!(files_of_owner().await, 1, "only the first upload remains");
    assert_eq!(s3.state.objects.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_failed_direct_upload_persists_cleanup_without_retaining_metadata() {
    let db = database().await;
    let s3 = FakeS3::start("owned-unit-fixture").await;
    s3.refuse_puts(true);
    s3.refuse_deletes(true);
    let error = file_operations::store_upload(
        &db.pool,
        &s3.store,
        db.user,
        UploadDeclaration {
            filename: "me.png",
            content_type: "image/png",
            file_size: PNG.len() as i64,
            reason: FileReason::Avatar,
            visibility: FileVisibility::Public,
            sha256_hex: &digest(PNG),
        },
        PNG.to_vec(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, UploadError::StorageUnavailable);
    let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files")
        .fetch_one(&db.raw)
        .await
        .unwrap();
    assert_eq!(files, 0, "failed uploads are inaccessible immediately");
    let object_key: String = sqlx::query_scalar("SELECT object_key FROM pending_object_deletions")
        .fetch_one(&db.raw)
        .await
        .unwrap();
    assert!(s3.object(&object_key).is_none());
    s3.refuse_deletes(false);
    sqlx::query("UPDATE pending_object_deletions SET delete_after=NOW()-INTERVAL '1 second'")
        .execute(&db.raw)
        .await
        .unwrap();
    file_operations::remove_expired_uploads(&db.pool, &s3.store)
        .await
        .unwrap();
    let cleanup: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_object_deletions")
        .fetch_one(&db.raw)
        .await
        .unwrap();
    assert_eq!(cleanup, 1, "S3 tombstones survive a successful deletion");
    // A PUT started before capability expiry may finish after that first DELETE.
    s3.put(&object_key, PNG);
    sqlx::query("UPDATE pending_object_deletions SET delete_after=NOW()-INTERVAL '1 second'")
        .execute(&db.raw)
        .await
        .unwrap();
    file_operations::remove_expired_uploads(&db.pool, &s3.store)
        .await
        .unwrap();
    assert!(s3.object(&object_key).is_none());
    assert!(files::due_object_deletions(&db.pool, 100)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_confirmation_distinguishes_an_unavailable_store_from_missing_bytes() {
    let db = database().await;
    let s3 = FakeS3::start("owned-unit-fixture").await;
    let file = reserve(&db, 7).await.unwrap();
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &s3.store, db.user, file.id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    s3.state.refuse_reads.store(true, Ordering::SeqCst);
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &s3.store, db.user, file.id)
            .await
            .unwrap_err(),
        ConfirmationError::StorageUnavailable
    );
    assert!(
        !files::get_file_by_id(&db.pool, file.id)
            .await
            .unwrap()
            .unwrap()
            .is_active
    );
    s3.state.refuse_reads.store(false, Ordering::SeqCst);
    s3.put(&file.object_key, b"fixture");
    file_operations::confirm_upload(&db.pool, &s3.store, db.user, file.id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_object_removal_failures_leave_durable_bounded_cleanup() {
    let db = database().await;
    limits(&db, 100, 50).await;
    let s3 = FakeS3::start("owned-unit-fixture").await;

    // Only the owner can delete.
    let file = reserve(&db, 7).await.unwrap();
    activate(&db, file.id).await;
    s3.put(&file.object_key, b"fixture");
    assert_eq!(
        file_operations::delete_owned_file(&db.pool, &s3.store, db.user + 1, file.id)
            .await
            .unwrap_err(),
        DeleteError::NotFound
    );
    assert!(s3.deleted().is_empty());

    // Metadata disappears immediately. A failed best-effort object removal leaves
    // persisted work that is not eligible until the issued PUT has expired.
    s3.refuse_deletes(true);
    file_operations::delete_owned_file(&db.pool, &s3.store, db.user, file.id)
        .await
        .unwrap();
    assert!(files::get_file_by_id(&db.pool, file.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(s3.deleted(), [s3.path(&file.object_key)]);
    s3.refuse_deletes(false);
    sqlx::query("UPDATE pending_object_deletions SET delete_after=NOW()-INTERVAL '1 second'")
        .execute(&db.raw)
        .await
        .unwrap();
    file_operations::remove_expired_uploads(&db.pool, &s3.store)
        .await
        .unwrap();
    assert_eq!(
        s3.deleted(),
        [s3.path(&file.object_key), s3.path(&file.object_key)]
    );

    // Expired metadata also disappears immediately while failed object work remains.
    s3.refuse_deletes(true);
    let stale = reserve(&db, 7).await.unwrap();
    s3.put(&stale.object_key, b"fixture");
    expire(&db, stale.id).await;
    assert_eq!(
        file_operations::remove_expired_uploads(&db.pool, &s3.store)
            .await
            .unwrap(),
        1
    );
    assert!(files::get_file_by_id(&db.pool, stale.id)
        .await
        .unwrap()
        .is_none());
    s3.refuse_deletes(false);
    assert_eq!(
        file_operations::remove_expired_uploads(&db.pool, &s3.store)
            .await
            .unwrap(),
        0
    );
    assert!(files::get_file_by_id(&db.pool, stale.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(s3.deleted().last(), Some(&s3.path(&stale.object_key)));
    assert_eq!(
        file_operations::remove_expired_uploads(&db.pool, &s3.store)
            .await
            .unwrap(),
        0
    );

    // Deleting a still-pending upload works the same way.
    let pending = reserve(&db, 7).await.unwrap();
    file_operations::delete_owned_file(&db.pool, &s3.store, db.user, pending.id)
        .await
        .unwrap();
    assert_eq!(s3.deleted().last(), Some(&s3.path(&pending.object_key)));
    assert!(files::get_file_by_id(&db.pool, pending.id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_put_that_lands_after_its_deadline_is_removed_by_the_queued_cleanup() {
    let db = database().await;
    let s3 = FakeS3::start("owned-unit-fixture").await;
    let stale = reserve(&db, 7).await.unwrap();
    // The window and the PUT deadline both pass with no bytes, and the sweep takes the record.
    sqlx::query("UPDATE files SET upload_expires_at = NOW() - INTERVAL '1 second', put_expires_at = NOW() - INTERVAL '1 second' WHERE id = $1")
        .bind(stale.id)
        .execute(&db.raw)
        .await
        .unwrap();
    assert_eq!(
        file_operations::remove_expired_uploads(&db.pool, &s3.store)
            .await
            .unwrap(),
        1
    );

    // A PUT accepted before the deadline lands after the record is gone.
    s3.put(&stale.object_key, b"fixture");
    assert_eq!(
        file_operations::remove_expired_uploads(&db.pool, &s3.store)
            .await
            .unwrap(),
        0
    );
    assert!(
        s3.object(&stale.object_key).is_some(),
        "late bytes wait out the grace period"
    );
    let queued_for_later: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pending_object_deletions WHERE object_key = $1 AND delete_after > NOW() + INTERVAL '30 minutes'",
    )
    .bind(&stale.object_key)
    .fetch_one(&db.raw)
    .await
    .unwrap();
    assert_eq!(queued_for_later, 1);

    // Once the queued time has come, the next sweep removes them.
    sqlx::query("UPDATE pending_object_deletions SET delete_after = NOW() - INTERVAL '1 second'")
        .execute(&db.raw)
        .await
        .unwrap();
    file_operations::remove_expired_uploads(&db.pool, &s3.store)
        .await
        .unwrap();
    assert!(s3.object(&stale.object_key).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_deletes_are_not_sent_while_every_storage_slot_is_busy() {
    let db = database().await;
    limits(&db, 100, 50).await;
    let s3 = FakeS3::start("owned-unit-fixture").await;
    let file = reserve(&db, 7).await.unwrap();
    activate(&db, file.id).await;
    s3.put(&file.object_key, b"fixture");

    let mut slots = Vec::new();
    while let Ok(permit) = s3.store.try_acquire_io() {
        slots.push(permit);
    }
    file_operations::delete_owned_file(&db.pool, &s3.store, db.user, file.id)
        .await
        .unwrap();
    assert!(
        s3.deleted().is_empty(),
        "no DELETE request is sent while every slot is busy"
    );
    assert!(s3.object(&file.object_key).is_some());
    drop(slots);

    sqlx::query("UPDATE pending_object_deletions SET delete_after=NOW()-INTERVAL '1 second'")
        .execute(&db.raw)
        .await
        .unwrap();
    file_operations::remove_expired_uploads(&db.pool, &s3.store)
        .await
        .unwrap();
    assert_eq!(s3.deleted(), [s3.path(&file.object_key)]);
    assert!(s3.object(&file.object_key).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_a_replaced_picture_is_removed_with_the_change_or_not_at_all() {
    let db = database().await;
    let old = reserve_avatar(&db, FileVisibility::Public).await;
    activate(&db, old.id).await;
    let new = reserve_avatar(&db, FileVisibility::Public).await;
    activate(&db, new.id).await;
    assert_eq!(
        db::users::accounts::set_user_avatar(&db.pool, db.user, Some(old.id))
            .await
            .unwrap(),
        db::users::accounts::UserAvatarOutcome::Applied(None)
    );

    // If the replaced picture cannot be removed, the change is refused as a whole.
    sqlx::query("CREATE FUNCTION refuse_picture_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'refused'; END $$")
        .execute(&db.raw)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER refuse_picture_delete BEFORE DELETE ON files FOR EACH ROW EXECUTE FUNCTION refuse_picture_delete()")
        .execute(&db.raw)
        .await
        .unwrap();
    assert!(
        db::users::accounts::set_user_avatar(&db.pool, db.user, Some(new.id))
            .await
            .is_err()
    );
    let pointer: Option<Uuid> =
        sqlx::query_scalar("SELECT avatar_file_id FROM users WHERE id = $1")
            .bind(db.user)
            .fetch_one(&db.raw)
            .await
            .unwrap();
    assert_eq!(pointer, Some(old.id));
    sqlx::query("DROP TRIGGER refuse_picture_delete ON files")
        .execute(&db.raw)
        .await
        .unwrap();

    // Otherwise the old row goes with the change, and its bytes are queued.
    assert_eq!(
        db::users::accounts::set_user_avatar(&db.pool, db.user, Some(new.id))
            .await
            .unwrap(),
        db::users::accounts::UserAvatarOutcome::Applied(Some(old.id))
    );
    assert!(files::get_file_by_id(&db.pool, old.id)
        .await
        .unwrap()
        .is_none());
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pending_object_deletions WHERE object_key = $1")
            .bind(format!("uploads/{}", old.id))
            .fetch_one(&db.raw)
            .await
            .unwrap();
    assert_eq!(queued, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_a_banned_account_keeps_its_picture() {
    let db = database().await;
    let old = reserve_avatar(&db, FileVisibility::Public).await;
    activate(&db, old.id).await;
    let new = reserve_avatar(&db, FileVisibility::Public).await;
    activate(&db, new.id).await;
    // The request was authenticated before the ban, so it still holds the account as it was.
    let user = db::users::accounts::get_user_by_id(&db.pool, db.user)
        .await
        .unwrap()
        .unwrap();
    avatars::set_user_avatar(&db.pool, &user, old.id)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET is_banned = TRUE WHERE id = $1")
        .bind(db.user)
        .execute(&db.raw)
        .await
        .unwrap();

    let changed = avatars::set_user_avatar(&db.pool, &user, new.id).await;
    assert_eq!(changed, Err(AvatarError::Denied));
    let cleared = avatars::remove_user_avatar(&db.pool, &user).await;
    assert_eq!(cleared, Err(AvatarError::Denied));
    let pointer: Option<Uuid> =
        sqlx::query_scalar("SELECT avatar_file_id FROM users WHERE id = $1")
            .bind(db.user)
            .fetch_one(&db.raw)
            .await
            .unwrap();
    assert_eq!(pointer, Some(old.id));
    assert!(files::get_file_by_id(&db.pool, old.id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_confirmation_waits_for_the_bytes_and_then_succeeds_once() {
    let db = database().await;
    let state = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(state.path(), None).await.unwrap();
    let reservation = file_operations::reserve_and_sign_upload(
        &db.pool,
        &store,
        db.user,
        UploadDeclaration {
            filename: "retry",
            content_type: "text/plain",
            file_size: 5,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(b"retry"),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        reservation.capability,
        UploadCapability::AuthenticatedLocalPut
    );
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &store, db.user, reservation.file_id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    file_operations::put_local_upload(
        &db.pool,
        &store,
        db.user,
        reservation.file_id,
        "text/plain",
        std::io::Cursor::new(b"retry"),
    )
    .await
    .unwrap();
    file_operations::confirm_upload(&db.pool, &store, db.user, reservation.file_id)
        .await
        .unwrap();
    assert!(
        files::get_file_by_id(&db.pool, reservation.file_id)
            .await
            .unwrap()
            .unwrap()
            .is_active
    );
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &store, db.user, reservation.file_id)
            .await
            .unwrap_err(),
        ConfirmationError::NotPending
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_pg_empty_nonempty_and_proposed_amount_aggregates() {
    let db = database().await;
    limits(&db, 10, 2).await;
    assert_eq!(
        files::get_user_total_storage(&db.pool, db.user)
            .await
            .unwrap(),
        0
    );
    let empty = files::get_user_file_stats(&db.pool, db.user)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (empty.total_files, empty.total_bytes, empty.uploads_24h),
        (0, 0, 0)
    );
    assert!(files::get_user_file_stats(&db.pool, i64::MAX)
        .await
        .unwrap()
        .is_none());
    let first = reserve(&db, 4).await.unwrap(); // empty NUMERIC aggregate
    let stats = files::get_user_file_stats(&db.pool, db.user)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.bytes_24h, 4);
    assert!(matches!(reserve(&db, 7).await, Err(AdmissionError::Quota)));
    let second = reserve(&db, 6).await.unwrap(); // exact byte/count boundary
    assert_ne!(first.id, second.id);
    assert!(matches!(reserve(&db, 1).await, Err(AdmissionError::Quota)));
    assert_eq!(
        files::get_user_file_stats(&db.pool, db.user)
            .await
            .unwrap()
            .unwrap()
            .bytes_24h,
        10
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn positive_bounded_size_missing_policy_and_numeric_overflow_fail_closed() {
    let db = database().await;
    limits(&db, i64::MAX, i32::MAX).await;
    for size in [i64::MIN, -1, 0, files::MAX_UPLOAD_BYTES + 1, i64::MAX] {
        assert!(matches!(
            reserve(&db, size).await,
            Err(AdmissionError::Invalid)
        ));
    }
    reserve(&db, files::MAX_UPLOAD_BYTES).await.unwrap();
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO upload_rate_limit(user_id,file_size,reason) VALUES ($1,$2,'other')",
        )
        .bind(db.user)
        .bind(i64::MAX)
        .execute(&db.raw)
        .await
        .unwrap();
    }
    // SUM exceeds i64::MAX; comparison is numeric and returns quota, not decode
    // failure, wraparound, clamping or acceptance.
    assert!(matches!(reserve(&db, 1).await, Err(AdmissionError::Quota)));
    sqlx::query("DELETE FROM file_reason_limits WHERE reason = 'other'")
        .execute(&db.raw)
        .await
        .unwrap();
    assert!(matches!(
        reserve(&db, 1).await,
        Err(AdmissionError::PolicyUnavailable)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_reservations_share_count_and_bytes_across_connections() {
    let db = Arc::new(database().await);
    limits(&db, 10, 5).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let db = db.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            reserve(&db, 2).await
        }));
    }
    let mut winners = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => winners += 1,
            Err(AdmissionError::Quota) => (),
            other => panic!("unexpected admission result: {other:?}"),
        }
    }
    assert_eq!(winners, 5);
    let stats = files::get_user_file_stats(&db.pool, db.user)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stats.uploads_24h, stats.bytes_24h), (5, 10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_and_deletion_do_not_refund_the_quota_but_the_window_does() {
    let db = database().await;
    limits(&db, 4, 1).await;
    let state = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(state.path(), None).await.unwrap();
    let item = reserve(&db, 4).await.unwrap();
    assert!(matches!(reserve(&db, 1).await, Err(AdmissionError::Quota)));
    expire(&db, item.id).await;
    assert!(matches!(reserve(&db, 1).await, Err(AdmissionError::Quota)));

    // Removing the expired record keeps the charge, so uploads cannot be
    // reserved over and over by letting them lapse.
    assert_eq!(
        file_operations::remove_expired_uploads(&db.pool, &store)
            .await
            .unwrap(),
        1
    );
    assert!(files::get_file_by_id(&db.pool, item.id)
        .await
        .unwrap()
        .is_none());
    assert!(matches!(reserve(&db, 1).await, Err(AdmissionError::Quota)));

    // Deleting a reservation by hand is no different.
    sqlx::query("UPDATE upload_rate_limit SET uploaded_at = NOW() - INTERVAL '2 days'")
        .execute(&db.raw)
        .await
        .unwrap();
    let next = reserve(&db, 4).await.unwrap();
    assert!(matches!(
        files::delete_file(&db.pool, next.id, db.user)
            .await
            .unwrap(),
        files::DeleteFileOutcome::Deleted(_)
    ));
    assert!(matches!(reserve(&db, 1).await, Err(AdmissionError::Quota)));

    // Only time passing frees the budget.
    sqlx::query("UPDATE upload_rate_limit SET uploaded_at = NOW() - INTERVAL '2 days'")
        .execute(&db.raw)
        .await
        .unwrap();
    reserve(&db, 4).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_activation_owner_and_pending_checks_are_exactly_once() {
    let db = Arc::new(database().await);
    let item = reserve(&db, 7).await.unwrap();
    assert!(
        !files::activate_upload(&db.pool, item.id, db.user + 1)
            .await
            .unwrap(),
        "only the owner can activate an upload"
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        let barrier = barrier.clone();
        let id = item.id;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            files::activate_upload(&db.pool, id, db.user).await.unwrap()
        }));
    }
    let mut winners = 0;
    for task in tasks {
        if task.await.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1);
    let active = files::get_file_by_id(&db.pool, item.id)
        .await
        .unwrap()
        .unwrap();
    assert!(active.is_active && active.confirmed_at.is_some());
    assert!(active.upload_expires_at.is_none());
    assert_eq!(
        files::get_user_total_storage(&db.pool, db.user)
            .await
            .unwrap(),
        7
    );
    assert_eq!(
        files::get_user_file_stats(&db.pool, db.user)
            .await
            .unwrap()
            .unwrap()
            .uploads_24h,
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_deletion_and_activation_only_apply_to_the_right_files() {
    let db = database().await;
    limits(&db, 100, 50).await;
    let active = reserve(&db, 7).await.unwrap();
    activate(&db, active.id).await;
    let stale = reserve(&db, 7).await.unwrap();
    let live = reserve(&db, 7).await.unwrap();
    expire(&db, stale.id).await;

    // An expired reservation cannot be activated and is the only file listed.
    assert!(!files::activate_upload(&db.pool, stale.id, db.user)
        .await
        .unwrap());
    let expired = files::expired_uploads(&db.pool, 10).await.unwrap();
    assert_eq!(
        expired.iter().map(|file| file.id).collect::<Vec<_>>(),
        [stale.id]
    );
    // Active and live files are never treated as expired.
    for id in [active.id, live.id] {
        assert!(files::delete_expired_upload(&db.pool, id)
            .await
            .unwrap()
            .is_none());
    }
    assert!(files::delete_expired_upload(&db.pool, stale.id)
        .await
        .unwrap()
        .is_some());
    assert!(files::delete_expired_upload(&db.pool, stale.id)
        .await
        .unwrap()
        .is_none());

    // Deleting a file needs its owner, and returns the record for cleanup.
    assert!(matches!(
        files::delete_file(&db.pool, active.id, db.user + 1)
            .await
            .unwrap(),
        files::DeleteFileOutcome::NotFound
    ));
    let files::DeleteFileOutcome::Deleted(removed) =
        files::delete_file(&db.pool, active.id, db.user)
            .await
            .unwrap()
    else {
        panic!("owner did not delete the file")
    };
    assert_eq!(removed.object_key, active.object_key);
    assert!(matches!(
        files::delete_file(&db.pool, active.id, db.user)
            .await
            .unwrap(),
        files::DeleteFileOutcome::NotFound
    ));
    assert!(
        files::get_user_file(&db.pool, live.id, db.user)
            .await
            .unwrap()
            .is_none(),
        "a pending upload is not a file yet"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downloads_are_owner_only_unless_the_file_is_a_public_avatar() {
    let db = database().await;
    let id = insert_active_file(&db, "other", "image/png", 8).await;
    assert!(files::get_downloadable_file(&db.pool, id, None)
        .await
        .unwrap()
        .is_none());
    assert!(
        files::get_downloadable_file(&db.pool, id, Some(db.user + 1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(files::get_downloadable_file(&db.pool, id, Some(db.user))
        .await
        .unwrap()
        .is_some());

    // An avatar nobody uses is as private as any other file ...
    sqlx::query("UPDATE files SET reason = 'avatar' WHERE id = $1")
        .bind(id)
        .execute(&db.raw)
        .await
        .unwrap();
    assert!(files::get_downloadable_file(&db.pool, id, None)
        .await
        .unwrap()
        .is_none());
    // ... until it becomes someone's avatar.
    sqlx::query("UPDATE users SET avatar_file_id = $1 WHERE id = $2")
        .bind(id)
        .bind(db.user)
        .execute(&db.raw)
        .await
        .unwrap();
    assert!(files::get_downloadable_file(&db.pool, id, None)
        .await
        .unwrap()
        .is_some());

    // Organization avatars work the same way.
    let org_avatar = insert_active_file(&db, "org_avatar", "image/png", 8).await;
    assert!(files::get_downloadable_file(&db.pool, org_avatar, None)
        .await
        .unwrap()
        .is_none());
    sqlx::query("INSERT INTO organizations(name, avatar_file_id) VALUES ('storage-org', $1)")
        .bind(org_avatar)
        .execute(&db.raw)
        .await
        .unwrap();
    assert!(files::get_downloadable_file(&db.pool, org_avatar, None)
        .await
        .unwrap()
        .is_some());

    // Declaring an upload public is only possible for avatars.
    let invalid = files::reserve_upload(
        &db.pool,
        ReserveUpload {
            user_id: db.user,
            filename: "asset",
            content_type: "text/plain",
            file_size: 1,
            reason: FileReason::Other,
            visibility: FileVisibility::Public,
            sha256_hex: &digest(b"x"),
        },
    )
    .await;
    assert!(matches!(invalid, Err(AdmissionError::Invalid)));
    let public = reserve_avatar(&db, FileVisibility::Public).await;
    assert!(
        files::get_downloadable_file(&db.pool, public.id, None)
            .await
            .unwrap()
            .is_none(),
        "still pending"
    );
    activate(&db, public.id).await;
    assert!(files::get_downloadable_file(&db.pool, public.id, None)
        .await
        .unwrap()
        .is_some());

    // Adding up sizes that do not fit in 64 bits fails instead of wrapping.
    for _ in 0..2 {
        insert_active_file(&db, "other", "text/plain", i64::MAX).await;
    }
    assert!(files::get_user_total_storage(&db.pool, db.user)
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_upload_admission_rechecks_committed_ban_and_password_reset() {
    let db = database().await;
    for restriction in ["is_banned", "must_change_password"] {
        sqlx::query(&format!("UPDATE users SET {restriction}=true WHERE id=$1"))
            .bind(db.user)
            .execute(&db.raw)
            .await
            .unwrap();
        assert!(matches!(
            reserve(&db, 7).await,
            Err(AdmissionError::Invalid)
        ));
        let counts: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM files),(SELECT COUNT(*) FROM upload_rate_limit)",
        )
        .fetch_one(&db.raw)
        .await
        .unwrap();
        assert_eq!(counts, (0, 0));
        sqlx::query(&format!("UPDATE users SET {restriction}=false WHERE id=$1"))
            .bind(db.user)
            .execute(&db.raw)
            .await
            .unwrap();
    }
    reserve(&db, 7).await.unwrap();
}

fn isolated_endpoint(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        u.scheme() == "http"
            && matches!(u.host_str(), Some("127.0.0.1" | "localhost"))
            && u.port()
                .is_some_and(|p| p >= 1024 && p != 9000 && p != 9001)
            && u.path() == "/"
            && u.query().is_none()
            && u.fragment().is_none()
            && u.username().is_empty()
            && u.password().is_none()
    })
}

fn isolated_bucket(value: &str) -> bool {
    value
        .strip_prefix("gitserv-storage-test-")
        .is_some_and(|suffix| {
            suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
}

#[test]
fn actual_fixture_refuses_default_production_or_unowned_targets() {
    assert!(isolated_endpoint("http://127.0.0.1:11082"));
    assert!(isolated_bucket(&format!(
        "gitserv-storage-test-{}",
        Uuid::new_v4().simple()
    )));
    for target in [
        "http://localhost:9000",
        "https://objects.example.com:11082",
        "http://127.0.0.1:11082/prod",
        "http://localhost:11082?token=secret",
    ] {
        assert!(!isolated_endpoint(target));
    }
    for target in [
        "gitserv-files",
        "production",
        "gitserv-storage-test-default",
    ] {
        assert!(!isolated_bucket(target));
    }
}

/// A real S3-compatible bucket that the environment says is a throwaway fixture.
struct ObjectFixture {
    config: S3Config,
    client: aws_sdk_s3::Client,
    store: ObjectStore,
    _state_dir: tempfile::TempDir,
}

impl ObjectFixture {
    async fn open() -> Self {
        let required = |name: &str| {
            std::env::var(name)
                .expect("actual S3 test requires all explicit isolated fixture variables")
        };
        let internal = required("GITSERV_STORAGE_TEST_ENDPOINT");
        let public = required("GITSERV_STORAGE_TEST_PUBLIC_ENDPOINT");
        let bucket = required("GITSERV_STORAGE_TEST_BUCKET");
        assert!(
            isolated_endpoint(&internal) && isolated_endpoint(&public),
            "refuse non-isolated/default S3 endpoint"
        );
        assert!(
            isolated_bucket(&bucket),
            "refuse production/default/unowned bucket"
        );
        let config = S3Config {
            endpoint: internal,
            public_endpoint: Some(public),
            access_key: required("GITSERV_STORAGE_TEST_ACCESS_KEY"),
            secret_key: required("GITSERV_STORAGE_TEST_SECRET_KEY"),
            bucket,
            region: "us-east-1".into(),
        };
        let client = s3::create_client(&config).await.unwrap();
        let marker = client
            .get_object()
            .bucket(&config.bucket)
            .key("_gitserv_fixture_owner")
            .send()
            .await
            .expect("required bucket ownership marker missing");
        let expected = format!("production-storage-v1:{}", config.bucket);
        assert_eq!(
            marker.content_length(),
            Some(i64::try_from(expected.len()).unwrap())
        );
        let body = marker.body.collect().await.unwrap().into_bytes();
        assert_eq!(
            body.as_ref(),
            expected.as_bytes(),
            "refuse ownership mismatch"
        );
        let state_dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(state_dir.path(), Some(config.clone()))
            .await
            .unwrap();
        Self {
            config,
            client,
            store,
            _state_dir: state_dir,
        }
    }

    /// Test teardown only: removes one object this test created.
    async fn remove(&self, key: &str) {
        assert!(key.starts_with("uploads/"));
        self.client
            .delete_object()
            .bucket(&self.config.bucket)
            .key(key)
            .send()
            .await
            .unwrap();
    }

    /// Writes an object directly with the fixture's own credentials, as a
    /// client that bypassed the presigned URL would.
    async fn write(&self, key: &str, content_type: &str, bytes: &[u8]) {
        assert!(key.starts_with("uploads/"));
        self.client
            .put_object()
            .bucket(&self.config.bucket)
            .key(key)
            .content_type(content_type)
            .body(aws_smithy_types::byte_stream::ByteStream::from(
                bytes.to_vec(),
            ))
            .send()
            .await
            .unwrap();
    }
}

async fn actual_put(
    http: &reqwest::Client,
    url: &str,
    headers: &BTreeMap<String, String>,
    data: &[u8],
) -> reqwest::Response {
    let mut request = http.put(url).body(data.to_vec());
    for (key, value) in headers {
        request = request.header(key, value);
    }
    request
        .send()
        .await
        .expect("actual public-authority PUT unavailable")
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit owned S3/RustFS fixture; absence is a failure, not a skip"]
async fn actual_presigned_put_replay_confirmation_download_and_delete() {
    let db = database().await;
    let object = ObjectFixture::open().await;
    let data = b"owned real public authority upload";
    let reservation = file_operations::reserve_and_sign_upload(
        &db.pool,
        &object.store,
        db.user,
        UploadDeclaration {
            filename: "fixture.txt",
            content_type: "text/plain",
            file_size: data.len() as i64,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(data),
        },
    )
    .await
    .unwrap();
    assert_eq!(reservation.capability, UploadCapability::S3DirectPut);
    let signed_url = url::Url::parse(&reservation.url).unwrap();
    let public = url::Url::parse(object.config.public_endpoint.as_deref().unwrap()).unwrap();
    assert_eq!(signed_url.origin(), public.origin());
    assert_ne!(
        url::Url::parse(&object.config.endpoint).unwrap().origin(),
        public.origin(),
        "this acceptance must exercise distinct internal/public authorities"
    );
    let id = reservation.file_id;
    let http = http_client();

    assert_eq!(
        file_operations::confirm_upload(&db.pool, &object.store, db.user, id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed,
        "nothing has been uploaded yet"
    );
    assert!(
        actual_put(&http, &reservation.url, &reservation.headers, data)
            .await
            .status()
            .is_success(),
        "real PUT must be accepted"
    );
    let replay = actual_put(&http, &reservation.url, &reservation.headers, data).await;
    assert!(
        matches!(replay.status().as_u16(), 409 | 412),
        "conditional PUT must reject replay, not a signing/auth error"
    );
    file_operations::confirm_upload(&db.pool, &object.store, db.user, id)
        .await
        .unwrap();

    let download = file_operations::download_for_user(&db.pool, &object.store, db.user, id)
        .await
        .unwrap();
    assert_eq!(download.capability, DownloadCapability::S3PresignedGet);
    let received = http.get(&download.url).send().await.unwrap();
    assert!(
        received.status().is_success(),
        "real public-authority GET must be accepted"
    );
    assert_eq!(
        received.headers()["content-disposition"],
        "attachment; filename*=UTF-8''fixture.txt",
        "uploaded files download, they are not displayed"
    );
    assert_eq!(received.bytes().await.unwrap().as_ref(), data);
    assert_eq!(
        file_operations::download_asset(&db.pool, &object.store, Some(db.user + 1), id)
            .await
            .unwrap_err(),
        DownloadError::NotFound
    );

    file_operations::delete_owned_file(&db.pool, &object.store, db.user, id)
        .await
        .unwrap();
    assert!(files::get_file_by_id(&db.pool, id).await.unwrap().is_none());
    assert_eq!(
        file_operations::download_for_user(&db.pool, &object.store, db.user, id)
            .await
            .unwrap_err(),
        DownloadError::NotFound
    );
    assert!(object
        .client
        .head_object()
        .bucket(&object.config.bucket)
        .key(&reservation.object_key)
        .send()
        .await
        .is_err());
    assert!(
        actual_put(&http, &reservation.url, &reservation.headers, data)
            .await
            .status()
            .is_success(),
        "the released signed PUT remains usable after confirmed metadata deletion"
    );
    sqlx::query("UPDATE pending_object_deletions SET delete_after=NOW()-INTERVAL '1 second'")
        .execute(&db.raw)
        .await
        .unwrap();
    file_operations::remove_expired_uploads(&db.pool, &object.store)
        .await
        .unwrap();
    assert!(object
        .client
        .head_object()
        .bucket(&object.config.bucket)
        .key(&reservation.object_key)
        .send()
        .await
        .is_err());

    let pending = file_operations::reserve_and_sign_upload(
        &db.pool,
        &object.store,
        db.user,
        UploadDeclaration {
            filename: "pending.txt",
            content_type: "text/plain",
            file_size: data.len() as i64,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(data),
        },
    )
    .await
    .unwrap();
    file_operations::delete_owned_file(&db.pool, &object.store, db.user, pending.file_id)
        .await
        .unwrap();
    assert!(
        actual_put(&http, &pending.url, &pending.headers, data)
            .await
            .status()
            .is_success(),
        "the released signed PUT remains usable after pending metadata deletion"
    );
    assert!(files::get_file_by_id(&db.pool, pending.file_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        file_operations::download_for_user(&db.pool, &object.store, db.user, pending.file_id,)
            .await
            .unwrap_err(),
        DownloadError::NotFound
    );
    sqlx::query("UPDATE pending_object_deletions SET delete_after=NOW()-INTERVAL '1 second'")
        .execute(&db.raw)
        .await
        .unwrap();
    file_operations::remove_expired_uploads(&db.pool, &object.store)
        .await
        .unwrap();
    assert!(object
        .client
        .head_object()
        .bucket(&object.config.bucket)
        .key(&pending.object_key)
        .send()
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit owned S3/RustFS fixture; absence is a failure, not a skip"]
async fn actual_wrong_size_type_checksum_and_changed_headers_are_refused() {
    let db = database().await;
    let object = ObjectFixture::open().await;
    let http = http_client();
    let data = b"expected";
    let reservation = file_operations::reserve_and_sign_upload(
        &db.pool,
        &object.store,
        db.user,
        UploadDeclaration {
            filename: "fixture.txt",
            content_type: "text/plain",
            file_size: data.len() as i64,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(data),
        },
    )
    .await
    .unwrap();

    // The signed type and checksum are enforced by the real store.
    let mut other_type = reservation.headers.clone();
    other_type.insert("content-type".into(), "application/octet-stream".into());
    assert!(!actual_put(&http, &reservation.url, &other_type, data)
        .await
        .status()
        .is_success());
    assert!(
        !actual_put(&http, &reservation.url, &reservation.headers, b"replaced")
            .await
            .status()
            .is_success(),
        "signed checksum must be enforced by real store"
    );
    // Refused requests do not use up the single permitted upload.
    assert!(
        actual_put(&http, &reservation.url, &reservation.headers, data)
            .await
            .status()
            .is_success()
    );
    file_operations::confirm_upload(&db.pool, &object.store, db.user, reservation.file_id)
        .await
        .unwrap();
    file_operations::delete_owned_file(&db.pool, &object.store, db.user, reservation.file_id)
        .await
        .unwrap();

    // Objects written around the presigned URL are still checked when confirmed.
    let wrong_size = file_operations::reserve_and_sign_upload(
        &db.pool,
        &object.store,
        db.user,
        UploadDeclaration {
            filename: "fixture.txt",
            content_type: "text/plain",
            file_size: data.len() as i64,
            reason: FileReason::Other,
            visibility: FileVisibility::Owner,
            sha256_hex: &digest(data),
        },
    )
    .await
    .unwrap();
    object
        .write(&wrong_size.object_key, "text/plain", b"wrong size")
        .await;
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &object.store, db.user, wrong_size.file_id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    object.remove(&wrong_size.object_key).await;

    let not_an_image = file_operations::reserve_and_sign_upload(
        &db.pool,
        &object.store,
        db.user,
        UploadDeclaration {
            filename: "me.png",
            content_type: "image/png",
            file_size: PNG.len() as i64,
            reason: FileReason::Avatar,
            visibility: FileVisibility::Public,
            sha256_hex: &digest(PNG),
        },
    )
    .await
    .unwrap();
    object
        .write(&not_an_image.object_key, "image/png", b"<html>!!")
        .await;
    assert_eq!(
        file_operations::confirm_upload(&db.pool, &object.store, db.user, not_an_image.file_id)
            .await
            .unwrap_err(),
        ConfirmationError::VerificationFailed
    );
    assert!(
        !files::get_file_by_id(&db.pool, not_an_image.file_id)
            .await
            .unwrap()
            .unwrap()
            .is_active
    );
    object.remove(&not_an_image.object_key).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit owned S3/RustFS fixture; absence is a failure, not a skip"]
async fn actual_equal_endpoints_and_public_avatar_still_work() {
    let db = database().await;
    let object = ObjectFixture::open().await;
    let mut config = object.config.clone();
    config.public_endpoint = None;
    let state_dir = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(state_dir.path(), Some(config))
        .await
        .unwrap();
    let data = base64::engine::general_purpose::STANDARD.decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aZ1cAAAAASUVORK5CYII=").unwrap();
    let reservation = file_operations::reserve_and_sign_upload(
        &db.pool,
        &store,
        db.user,
        UploadDeclaration {
            filename: "fixture.png",
            content_type: "image/png",
            file_size: data.len() as i64,
            reason: FileReason::Avatar,
            visibility: FileVisibility::Public,
            sha256_hex: &digest(&data),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        url::Url::parse(&reservation.url).unwrap().origin(),
        url::Url::parse(&object.config.endpoint).unwrap().origin()
    );
    let http = http_client();
    assert!(
        actual_put(&http, &reservation.url, &reservation.headers, &data)
            .await
            .status()
            .is_success()
    );
    file_operations::confirm_upload(&db.pool, &store, db.user, reservation.file_id)
        .await
        .unwrap();
    assert!(
        files::get_downloadable_file(&db.pool, reservation.file_id, None)
            .await
            .unwrap()
            .is_some()
    );
    let shown = file_operations::download_asset(&db.pool, &store, None, reservation.file_id)
        .await
        .unwrap();
    let received = http.get(&shown.url).send().await.unwrap();
    assert!(received.status().is_success());
    assert_eq!(received.headers().get("content-type").unwrap(), "image/png");
    assert_eq!(received.bytes().await.unwrap().as_ref(), data.as_slice());
    object.remove(&reservation.object_key).await;
}
