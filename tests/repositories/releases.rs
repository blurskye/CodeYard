//! Releases through the JSON API and the native forms, the authorization matrix for drafts and
//! private repositories, uploaded assets, source archives and the watcher notification. Each
//! scenario runs on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::sync::Arc;

/// `alice/proj` is public, `alice/vault` is private. Alice owns both, bob reads the public one
/// only, carol can read neither.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    alice: Client,
    bob: Client,
    carol: Client,
    anonymous: Client,
    main: String,
}

async fn setup(pg: bool) -> World {
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
    let carol_id = database.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    for name in ["proj", "vault"] {
        codeyard::backend::repositories::lifecycle::provision(
            (database.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            name,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    }
    database
        .execute(
            "UPDATE repositories SET is_private = TRUE WHERE name = 'vault'",
            &[],
        )
        .await;
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, database.pool(), alice_id).await;
    let main = alice.save("main", "README.md", "unborn").await;
    alice.save("main", "docs.txt", &main).await;
    let bob = Client::login(&router, database.pool(), bob_id).await;
    let carol = Client::login(&router, database.pool(), carol_id).await;
    let anonymous = Client::anonymous(&router);
    World {
        database,
        _root: root,
        alice,
        bob,
        carol,
        anonymous,
        main,
    }
}

/// A multipart body with the CSRF token, a label and one file part named `file`.
async fn upload(
    client: &Client,
    path: &str,
    label: &str,
    name: &str,
    bytes: &[u8],
) -> pulls::Reply {
    let boundary = "----release-asset-boundary";
    let mut body = Vec::new();
    for (field, value) in [("csrf_token", client.csrf.as_str()), ("label", label)] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    client
        .exchange(
            "POST",
            path,
            &format!("multipart/form-data; boundary={boundary}"),
            body,
        )
        .await
}

async fn releases_flow(pg: bool) {
    let mut world = setup(pg).await;
    let (alice, bob, carol, anonymous) = (&world.alice, &world.bob, &world.carol, &world.anonymous);
    // The vault has a commit on main, so releases can target it.
    alice
        .expect(
            "POST",
            "/api/repos/alice/vault/contents/README.md",
            json!({"content": "eA==", "message": "Add README", "branch": "main", "expected_revision": "unborn"}),
            StatusCode::CREATED,
        )
        .await;
    bob.expect(
        "PUT",
        "/api/repos/alice/proj/watch",
        Value::Null,
        StatusCode::OK,
    )
    .await;

    // Publishing creates the tag at the target and marks the release latest.
    let first = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v1.0", "target_commitish": "main", "name": "First", "body": "Notes"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(first["tag_name"], "v1.0", "{first}");
    assert_eq!(first["latest"], true, "{first}");
    assert_eq!(first["draft"], false, "{first}");
    assert!(first["html_url"]
        .as_str()
        .unwrap()
        .ends_with("/alice/proj/releases/tag/v1.0"));
    let tags = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/tags",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert!(tags.to_string().contains("v1.0"), "{tags}");
    let first_id = first["id"].as_i64().unwrap();

    // A draft is visible to writers only.
    let draft = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v2.0-rc", "target_commitish": world.main, "draft": true}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(draft["latest"], false, "{draft}");
    let draft_id = draft["id"].as_i64().unwrap();
    let listed = bob
        .expect(
            "GET",
            "/api/repos/alice/proj/releases",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/releases",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");
    bob.expect(
        "GET",
        "/api/repos/alice/proj/releases/tags/v2.0-rc",
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    alice
        .expect(
            "GET",
            "/api/repos/alice/proj/releases/tags/v2.0-rc",
            Value::Null,
            StatusCode::OK,
        )
        .await;

    // Readers and outsiders cannot publish; a duplicate tag is refused.
    let (status, _) = bob
        .api(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v9", "target_commitish": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = anonymous
        .api(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v9", "target_commitish": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, value) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v1.0", "target_commitish": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");

    // A new latest release takes the flag from the previous one.
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v1.1", "target_commitish": "main", "make_latest": true}),
            StatusCode::CREATED,
        )
        .await;
    let first = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/releases/{first_id}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(first["latest"], false, "{first}");

    // Assets: upload through the form, download as a reader, and count the download.
    let form_path = format!("/alice/proj/releases/{first_id}/assets");
    let reply = upload(
        alice,
        &form_path,
        "Linux build",
        "tool.bin",
        b"binary bytes",
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let downloaded = bob
        .exchange(
            "GET",
            "/alice/proj/releases/download/v1.0/tool.bin",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(downloaded.status, StatusCode::OK);
    assert_eq!(downloaded.body, b"binary bytes");
    let release = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/releases/{first_id}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(release["assets"][0]["download_count"], 1, "{release}");
    let asset_id = release["assets"][0]["id"].as_i64().unwrap();
    let duplicate = upload(alice, &form_path, "", "tool.bin", b"again").await;
    assert_eq!(
        duplicate.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        duplicate.text()
    );
    let outsider = carol
        .exchange(
            "GET",
            "/alice/proj/releases/download/v1.0/tool.bin",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(
        outsider.status,
        StatusCode::OK,
        "public assets are readable by any reader"
    );
    let hidden = alice
        .exchange(
            "GET",
            "/alice/proj/releases/download/v2.0-rc/tool.bin",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    let reply = bob
        .form_reply(
            &format!("/alice/proj/releases/{first_id}/assets/{asset_id}/delete"),
            &[],
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = alice
        .form_reply(
            &format!("/alice/proj/releases/{first_id}/assets/{asset_id}/delete"),
            &[],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let gone = bob
        .exchange(
            "GET",
            "/alice/proj/releases/download/v1.0/tool.bin",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    // Pages render for readers, with the source archives and the draft hidden from them.
    let (status, html) = bob.page("/alice/proj/releases").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("v1.1") && !html.contains("v2.0-rc"), "{html}");
    let (status, html) = bob.page("/alice/proj/releases/tag/v1.0").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("/alice/proj/archive/v1.0.zip"), "{html}");
    let (status, _) = bob.page("/alice/proj/releases/tag/v2.0-rc").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, html) = alice.page("/alice/proj/releases/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("name=\"tag\""), "{html}");
    let (status, _) = bob.page("/alice/proj/releases/new").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Edit and delete from the native forms. Deleting removes the draft and its tag stays.
    let reply = alice
        .form_reply(
            &format!("/alice/proj/releases/edit/{draft_id}"),
            &[
                ("title", "Release candidate"),
                ("body", "Testing"),
                ("latest", "on"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let published = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/releases/{draft_id}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(published["draft"], false, "{published}");
    assert_eq!(published["name"], "Release candidate", "{published}");
    let reply = alice
        .form_reply(&format!("/alice/proj/releases/{draft_id}/delete"), &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/releases/{draft_id}"),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    alice
        .expect(
            "PATCH",
            &format!("/api/repos/alice/proj/releases/{first_id}"),
            json!({"prerelease": true}),
            StatusCode::OK,
        )
        .await;
    alice
        .expect(
            "DELETE",
            &format!("/api/repos/alice/proj/releases/{first_id}"),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    alice
        .expect(
            "GET",
            "/api/repos/alice/proj/releases/tags/v1.0",
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    let tags = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/tags",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert!(
        tags.to_string().contains("v1.0"),
        "the tag outlives its release: {tags}"
    );

    // Private repositories: outsiders and readers without access see nothing.
    alice
        .expect(
            "POST",
            "/api/repos/alice/vault/releases",
            json!({"tag_name": "v1", "target_commitish": "main"}),
            StatusCode::CREATED,
        )
        .await;
    bob.expect(
        "GET",
        "/api/repos/alice/vault/releases",
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    anonymous
        .expect(
            "GET",
            "/api/repos/alice/vault/releases",
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    let (status, _) = anonymous.page("/alice/vault/releases").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    world.database.finish().await;
}

async fn archives(pg: bool) {
    let mut world = setup(pg).await;
    let zip = world
        .anonymous
        .exchange(
            "GET",
            "/alice/proj/archive/main.zip",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(zip.status, StatusCode::OK);
    assert!(zip.body.starts_with(b"PK"));
    assert!(String::from_utf8_lossy(&zip.body).contains("proj-main/docs.txt"));
    let targz = world
        .anonymous
        .exchange(
            "GET",
            "/alice/proj/archive/main.tar.gz",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(targz.status, StatusCode::OK);
    assert_eq!(&targz.body[..2], &[0x1f, 0x8b]);
    let by_commit = world
        .anonymous
        .exchange(
            "GET",
            &format!("/alice/proj/archive/{}.zip", world.main),
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(by_commit.status, StatusCode::OK);
    let missing = world
        .anonymous
        .exchange(
            "GET",
            "/alice/proj/archive/no-such-ref.zip",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let unknown = world
        .anonymous
        .exchange(
            "GET",
            "/alice/proj/archive/main.rar",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let private = world
        .anonymous
        .exchange(
            "GET",
            "/alice/vault/archive/main.zip",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(private.status, StatusCode::NOT_FOUND);
    world.database.finish().await;
}

async fn publish_notifies_release_watchers(pg: bool) {
    let mut world = setup(pg).await;
    world
        .bob
        .expect(
            "PUT",
            "/api/repos/alice/proj/watch",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let reply = world
        .bob
        .form_reply(
            "/alice/proj/_forms/watch-level",
            &[("level", "custom"), ("events_releases", "on")],
        )
        .await;
    assert!(
        reply.status.is_redirection() || reply.status.is_success(),
        "{}",
        reply.text()
    );
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v3.0", "target_commitish": "main", "name": "Third"}),
            StatusCode::CREATED,
        )
        .await;
    let inbox = world
        .bob
        .expect("GET", "/api/notifications", Value::Null, StatusCode::OK)
        .await;
    let found = inbox["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["kind"] == "release_published");
    assert!(found, "{inbox}");
    world.database.finish().await;
}

#[tokio::test]
async fn postgres_releases_flow() {
    releases_flow(true).await;
}

#[tokio::test]
async fn sqlite_releases_flow() {
    releases_flow(false).await;
}

#[tokio::test]
async fn postgres_source_archives() {
    archives(true).await;
}

#[tokio::test]
async fn sqlite_source_archives() {
    archives(false).await;
}

#[tokio::test]
async fn postgres_release_publish_notifies_release_watchers() {
    publish_notifies_release_watchers(true).await;
}

#[tokio::test]
async fn sqlite_release_publish_notifies_release_watchers() {
    publish_notifies_release_watchers(false).await;
}
