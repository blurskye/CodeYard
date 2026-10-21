//! Releases through the JSON API and the native forms, the authorization matrix for drafts and
//! private repositories, uploaded assets, source archives and the watcher notification. Each
//! scenario runs on SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as pulls;
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
    upload_reads: Arc<tokio::sync::Semaphore>,
}

async fn setup(pg: bool) -> World {
    bundle::install();
    let database = Database::new(pg).await;
    database.complete_setup().await;
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
    let state = AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let upload_reads = state.upload_reads.clone();
    let router = create_router(state);
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
        upload_reads,
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

async fn release_creation_uses_the_checked_repository_id(pg: bool) {
    use codeyard::backend::repositories::{
        lifecycle,
        releases::{self, ReleaseInput},
    };
    let mut world = setup(pg).await;
    let pool = world.database.pool();
    let original = db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = original.owner_user_id.unwrap();
    world
        .database
        .execute(
            "UPDATE repositories SET name='moved', is_private=TRUE WHERE id=$1",
            &[original.id],
        )
        .await;
    let replacement = lifecycle::provision(
        (pool, None),
        world._root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let state = AppState::new(
        pool.clone(),
        world._root.path().to_owned(),
        Arc::new(ObjectStore::open(world._root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let release = releases::create(
        pool,
        None,
        &state.ci_runs,
        &state.notification_broadcaster,
        actor,
        "alice",
        "proj",
        &ReleaseInput {
            repository_id: original.id,
            tag: "private-release".into(),
            target: world.main.clone(),
            fields: db::releases::ReleaseFields {
                body: Some("Confidential release notes".into()),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    assert!(
        db::releases::get_release_by_tag(pool, original.id, &release.tag_name)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db::releases::get_release_by_tag(pool, replacement.id, &release.tag_name)
            .await
            .unwrap()
            .is_none()
    );
    world.database.finish().await;
}
#[tokio::test]
async fn sqlite_release_creation_uses_the_checked_repository_id() {
    release_creation_uses_the_checked_repository_id(false).await;
}
#[tokio::test]
async fn postgres_release_creation_uses_the_checked_repository_id() {
    release_creation_uses_the_checked_repository_id(true).await;
}
mod regressions;

async fn long_release_target(pg: bool) {
    let mut world = setup(pg).await;
    let branch = format!("{}/{}", "a".repeat(128), "b".repeat(126));
    assert_eq!(branch.len(), 255);
    world.alice.branch(&branch, &world.main).await;
    let release = world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "long-target", "target_commitish": branch}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(release["target_commitish"], world.main);
    let oversized = format!("{branch}b");
    world.alice.branch(&oversized, &world.main).await;
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "too-long-target", "target_commitish": oversized}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    let repository =
        db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
            .await
            .unwrap()
            .unwrap();
    assert!(
        !codeyard::backend::repositories::store::repo_store(&repository, None)
            .refs()
            .await
            .unwrap()
            .contains_key("refs/tags/too-long-target")
    );
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_release_accepts_a_long_branch_target() {
    long_release_target(false).await;
}

#[tokio::test]
async fn postgres_release_accepts_a_long_branch_target() {
    long_release_target(true).await;
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
    assert!(first["tarball_url"]
        .as_str()
        .unwrap()
        .ends_with("/archive/refs%2Ftags%2Fv1.0.tar.gz"));
    assert!(first["zipball_url"]
        .as_str()
        .unwrap()
        .ends_with("/archive/refs%2Ftags%2Fv1.0.zip"));
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
    let listed_assets = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/releases?per_page=100",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        listed_assets
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == first_id)
            .unwrap()["assets"],
        release["assets"]
    );
    assert!(listed_assets
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["id"] != first_id)
        .all(|row| row["assets"].as_array().unwrap().is_empty()));
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
    let (model_status, model) = bob.model("/alice/proj/releases").await;
    assert_eq!(model_status, StatusCode::OK);
    let codeyard::webui::app::Page::Repositories(
        codeyard::webui::pages::repos::page::Page::Releases {
            page: codeyard::webui::pages::repos::releases::Page::List { items, .. },
            ..
        },
    ) = model
    else {
        panic!("not a release list")
    };
    assert!(items.iter().all(|item| item.body.is_empty()));
    let (status, html) = bob.page("/alice/proj/releases").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("v1.1") && !html.contains("v2.0-rc"), "{html}");
    let (status, html) = bob.page("/alice/proj/releases/tag/v1.0").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("/alice/proj/archive/refs%2Ftags%2Fv1.0.zip"),
        "{html}"
    );
    let (status, _) = bob.page("/alice/proj/releases/tag/v2.0-rc").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, html) = alice.page("/alice/proj/releases/new").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("name=\"tag\""), "{html}");
    assert!(html.contains("name=\"repository_id\""), "{html}");
    let stale = alice
        .form_reply(
            "/alice/proj/releases/new",
            &[
                ("repository_id", "0"),
                ("tag", "stale-release"),
                ("target", "main"),
                ("body", "private notes"),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    let missing = alice.page("/alice/proj/releases/tag/stale-release").await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    let (status, _) = bob.page("/alice/proj/releases/new").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Edit and delete from the native forms. Deleting removes the draft and its tag stays.
    let (_, edit) = alice
        .page(&format!("/alice/proj/releases/edit/{draft_id}"))
        .await;
    let revision = edit
        .split_once("name=\"revision\" value=\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap();
    let reply = alice
        .form_reply(
            &format!("/alice/proj/releases/edit/{draft_id}"),
            &[
                ("revision", revision),
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

async fn tag_archives_download_the_tag_even_when_a_branch_has_its_name(pg: bool) {
    let mut world = setup(pg).await;
    // The branch `v1.0` has a file that the tag, which points at the first commit, does not.
    world.alice.branch("v1.0", &world.main).await;
    world
        .alice
        .save("v1.0", "branch-only.txt", &world.main)
        .await;
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v1.0", "target_commitish": world.main, "name": "First"}),
            StatusCode::CREATED,
        )
        .await;
    let (status, page) = world.anonymous.page("/alice/proj/releases/tag/v1.0").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("/alice/proj/archive/refs%2Ftags%2Fv1.0.zip"),
        "{page}"
    );
    let tag = world
        .anonymous
        .exchange(
            "GET",
            "/alice/proj/archive/refs%2Ftags%2Fv1.0.zip",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(tag.status, StatusCode::OK);
    let tag_zip = String::from_utf8_lossy(&tag.body);
    assert!(tag_zip.contains("proj-v1.0/README.md"), "{tag_zip}");
    assert!(!tag_zip.contains("branch-only.txt"), "{tag_zip}");
    // The short name still names the branch, as Git resolves it.
    let branch = world
        .anonymous
        .exchange(
            "GET",
            "/alice/proj/archive/v1.0.zip",
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(branch.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&branch.body).contains("branch-only.txt"));
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
async fn releases_do_not_read_every_tag_annotation_to_find_one_tag() {
    let world = setup(false).await;
    let repository =
        db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
            .await
            .unwrap()
            .unwrap();
    let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let target = bare
        .find_commit(git2::Oid::from_str(&world.main).unwrap())
        .unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.invalid").unwrap();
    // Simulate tags already present before aggregate admission was enforced.
    for index in 0..5 {
        bare.tag(
            &format!("large-{index}"),
            target.as_object(),
            &signature,
            &"x".repeat(codeyard::backend::git::MAX_TAG_MESSAGE_BYTES),
            false,
        )
        .unwrap();
    }
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "large-0", "name": "Existing tag"}),
            StatusCode::CREATED,
        )
        .await;
}

#[tokio::test]
async fn postgres_releases_flow() {
    releases_flow(true).await;
}

#[tokio::test]
async fn sqlite_releases_flow() {
    releases_flow(false).await;
}

/// Publishes `tag` on `main` of alice's project and returns the release id.
async fn publish(alice: &Client, tag: &str) -> i64 {
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": tag, "target_commitish": "main"}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap()
}

/// An asset may be as large as the asset limit, which is larger than the form body limit.
async fn large_assets_upload(pg: bool) {
    let mut world = setup(pg).await;
    let release_id = publish(&world.alice, "v1.0").await;
    let uploaded = upload(
        &world.alice,
        &format!("/alice/proj/releases/{release_id}/assets"),
        "",
        "large.bin",
        &vec![7u8; 5 * 1024 * 1024],
    )
    .await;
    assert_eq!(
        uploaded.status,
        StatusCode::SEE_OTHER,
        "{}",
        uploaded.text()
    );
    let refused = upload(
        &world.bob,
        &format!("/alice/proj/releases/{release_id}/assets"),
        "",
        "reader.bin",
        &vec![7u8; 5 * 1024 * 1024],
    )
    .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.text());
    // A visitor who is not signed in is refused before the body is read.
    let refused = upload(
        &world.anonymous,
        &format!("/alice/proj/releases/{release_id}/assets"),
        "",
        "large.bin",
        &vec![7u8; 5 * 1024 * 1024],
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        refused.text()
    );
    world.database.finish().await;
}

/// The file of an asset leaves with the asset, through its release. Deleting the file directly is
/// refused, even for the account that uploaded it.
async fn asset_files_leave_through_their_release(pg: bool) {
    let mut world = setup(pg).await;
    let alice = &world.alice;
    let release_id = publish(alice, "v1.0").await;
    let uploaded = upload(
        alice,
        &format!("/alice/proj/releases/{release_id}/assets"),
        "",
        "tool.bin",
        b"binary bytes",
    )
    .await;
    assert_eq!(
        uploaded.status,
        StatusCode::SEE_OTHER,
        "{}",
        uploaded.text()
    );
    let assets = db::releases::list_release_assets(world.database.pool(), release_id)
        .await
        .unwrap();
    let file = assets[0].file_id;
    let refused = alice
        .exchange(
            "DELETE",
            &format!("/api/files/{file}"),
            "application/json",
            Vec::new(),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.text());
    let release = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/releases/{release_id}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(release["assets"].as_array().unwrap().len(), 1, "{release}");
    let asset_id = release["assets"][0]["id"].as_i64().unwrap();
    let removed = alice
        .form_reply(
            &format!("/alice/proj/releases/{release_id}/assets/{asset_id}/delete"),
            &[],
        )
        .await;
    assert!(removed.status.is_redirection(), "{}", removed.text());
    let remaining = db::releases::list_release_assets(world.database.pool(), release_id)
        .await
        .unwrap();
    assert!(remaining.is_empty());
    world.database.finish().await;
}

async fn release_validation_and_atomic_files(pg: bool) {
    let mut world = setup(pg).await;
    let pool = world.database.pool();
    let repo = db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = repo.owner_user_id.unwrap();
    for (field, value) in [("name", "bad\0title"), ("body", "bad\0notes")] {
        world
            .alice
            .expect(
                "POST",
                "/api/repos/alice/proj/releases",
                json!({"tag_name": "invalid", "target_commitish": "main", field: value}),
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await;
    }
    let long_branch = format!("{}/{}", "a".repeat(128), "b".repeat(128));
    world.alice.branch(&long_branch, &world.main).await;
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "invalid", "target_commitish": long_branch}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    let store = codeyard::backend::repositories::store::repo_store(&repo, None);
    assert!(!store
        .refs()
        .await
        .unwrap()
        .contains_key("refs/tags/invalid"));

    // Publication refuses a tag set over the metadata budget, even for a lightweight tag.
    let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let object = git
        .find_object(git2::Oid::from_str(&world.main).unwrap(), None)
        .unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    for i in 0..5 {
        git.tag(
            &format!("large-{i}"),
            &object,
            &signature,
            &"x".repeat(220 * 1024),
            false,
        )
        .unwrap();
    }
    world
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/releases",
            json!({"tag_name": "v-small", "target_commitish": "main"}),
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    assert!(git.find_reference("refs/tags/v-small").is_err());
    git.find_reference("refs/tags/large-4")
        .unwrap()
        .delete()
        .unwrap();
    // Unrelated annotations within the budget do not prevent publishing a release.
    let release_id = publish(&world.alice, "v-small").await;
    let refused = upload(
        &world.alice,
        &format!("/alice/proj/releases/{release_id}/assets"),
        "bad\0label",
        "bad.bin",
        b"x",
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    let file = db::files::reserve_upload(
        pool,
        db::files::ReserveUpload {
            user_id: actor,
            filename: "tool.bin",
            content_type: "application/octet-stream",
            file_size: 1,
            reason: db::models::FileReason::ReleaseAsset,
            visibility: db::models::FileVisibility::Owner,
            sha256_hex: &"a".repeat(64),
        },
    )
    .await
    .unwrap();
    let input = db::releases::NewReleaseAsset {
        file_id: file.id,
        name: "tool.bin",
        label: None,
        content_type: "application/octet-stream",
        size: 1,
    };
    // Unverified uploads cannot be attached, and a refused attachment leaves expiry intact.
    assert!(
        db::releases::add_release_asset(pool, actor, repo.id, release_id, &input)
            .await
            .is_err()
    );
    let pending = db::files::get_file_by_id(pool, file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!pending.is_active && pending.upload_expires_at.is_some());
    db::files::verify_pending_upload(pool, file.id, actor)
        .await
        .unwrap();
    let asset = db::releases::add_release_asset(pool, actor, repo.id, release_id, &input)
        .await
        .unwrap();
    let active = db::files::get_file_by_id(pool, file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(active.is_active && active.upload_expires_at.is_none());
    // Calling the native delete alone must remove metadata and persist cleanup.
    let removed = db::releases::delete_release_asset(pool, actor, repo.id, release_id, asset.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(removed.id, file.id);
    assert!(db::files::get_file_by_id(pool, file.id)
        .await
        .unwrap()
        .is_none());
    assert!(
        !db::files::finish_object_deletion(pool, &file.object_key)
            .await
            .unwrap(),
        "PUT grace keeps the cleanup queued"
    );
    world
        .database
        .execute(
            "UPDATE pending_object_deletions SET delete_after='2000-01-01T00:00:00Z'",
            &[],
        )
        .await;
    assert!(db::files::due_object_deletions(pool, 100)
        .await
        .unwrap()
        .contains(&file.object_key));

    let file = db::files::reserve_upload(
        pool,
        db::files::ReserveUpload {
            user_id: actor,
            filename: "second.bin",
            content_type: "application/octet-stream",
            file_size: 1,
            reason: db::models::FileReason::ReleaseAsset,
            visibility: db::models::FileVisibility::Owner,
            sha256_hex: &"b".repeat(64),
        },
    )
    .await
    .unwrap();
    db::files::verify_pending_upload(pool, file.id, actor)
        .await
        .unwrap();
    db::releases::add_release_asset(
        pool,
        actor,
        repo.id,
        release_id,
        &db::releases::NewReleaseAsset {
            file_id: file.id,
            ..input
        },
    )
    .await
    .unwrap();
    let removed = db::releases::delete_release(pool, actor, repo.id, release_id)
        .await
        .unwrap();
    assert_eq!(removed[0].id, file.id);
    assert!(db::files::get_file_by_id(pool, file.id)
        .await
        .unwrap()
        .is_none());
    world
        .database
        .execute(
            "UPDATE pending_object_deletions SET delete_after='2000-01-01T00:00:00Z'",
            &[],
        )
        .await;
    assert!(db::files::due_object_deletions(pool, 100)
        .await
        .unwrap()
        .contains(&file.object_key));
    world.database.finish().await;
}

#[tokio::test]
async fn release_validation_and_atomic_files_on_both_engines() {
    release_validation_and_atomic_files(false).await;
    release_validation_and_atomic_files(true).await;
}

#[tokio::test]
async fn postgres_large_assets_upload() {
    large_assets_upload(true).await;
}

#[tokio::test]
async fn sqlite_large_assets_upload() {
    large_assets_upload(false).await;
}

async fn repository_deletion_removes_asset_uploads(pg: bool) {
    let mut world = setup(pg).await;
    let release = publish(&world.alice, "v1.0").await;
    let uploaded = upload(
        &world.alice,
        &format!("/alice/proj/releases/{release}/assets"),
        "",
        "tool.bin",
        b"asset bytes",
    )
    .await;
    assert_eq!(
        uploaded.status,
        StatusCode::SEE_OTHER,
        "{}",
        uploaded.text()
    );
    let file_id = db::releases::list_release_assets(world.database.pool(), release)
        .await
        .unwrap()[0]
        .file_id;
    let file = db::files::get_file_by_id(world.database.pool(), file_id)
        .await
        .unwrap()
        .unwrap();
    let repository =
        db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
            .await
            .unwrap()
            .unwrap();
    codeyard::backend::repositories::lifecycle::delete(
        world.database.pool(),
        None,
        repository.owner_id(),
        repository.id,
    )
    .await
    .unwrap();
    assert!(db::files::get_file_by_id(world.database.pool(), file_id)
        .await
        .unwrap()
        .is_none());
    let queued: bool = match world.database.pool() {
        db::DbPool::Postgres(pool) => sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pending_object_deletions WHERE object_key=$1)",
        )
        .bind(&file.object_key)
        .fetch_one(pool)
        .await
        .unwrap(),
        db::DbPool::Sqlite(pool) => sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pending_object_deletions WHERE object_key=?1)",
        )
        .bind(&file.object_key)
        .fetch_one(pool)
        .await
        .unwrap(),
    };
    assert!(queued);
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_repository_deletion_removes_release_uploads() {
    repository_deletion_removes_asset_uploads(false).await;
}

#[tokio::test]
async fn postgres_repository_deletion_removes_release_uploads() {
    repository_deletion_removes_asset_uploads(true).await;
}

#[tokio::test]
async fn postgres_asset_files_leave_through_their_release() {
    asset_files_leave_through_their_release(true).await;
}

#[tokio::test]
async fn sqlite_asset_files_leave_through_their_release() {
    asset_files_leave_through_their_release(false).await;
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
async fn postgres_tag_archives_name_the_tag() {
    tag_archives_download_the_tag_even_when_a_branch_has_its_name(true).await;
}

#[tokio::test]
async fn sqlite_tag_archives_name_the_tag() {
    tag_archives_download_the_tag_even_when_a_branch_has_its_name(false).await;
}

#[tokio::test]
async fn postgres_release_publish_notifies_release_watchers() {
    publish_notifies_release_watchers(true).await;
}

#[tokio::test]
async fn sqlite_release_publish_notifies_release_watchers() {
    publish_notifies_release_watchers(false).await;
}

async fn release_write_regressions(pg: bool) {
    use axum::{
        body::Body,
        http::{header, Request},
    };
    use tower::ServiceExt;
    let mut world = setup(pg).await;
    let id = publish(&world.alice, "partial").await;
    let api = format!("/api/repos/alice/proj/releases/{id}");
    let title = json!({"name":"Concurrent title"});
    let notes = json!({"body":"Concurrent notes"});
    let (first, second) = tokio::join!(
        world.alice.api("PATCH", &api, title),
        world.alice.api("PATCH", &api, notes)
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    let saved = world
        .alice
        .expect("GET", &api, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(saved["name"], "Concurrent title");
    assert_eq!(saved["body"], "Concurrent notes");

    let pending = || {
        Body::from_stream(futures_util::stream::pending::<
            Result<bytes::Bytes, std::io::Error>,
        >())
    };
    let request = |client: &Client| {
        Request::builder()
            .method("POST")
            .uri(format!("/alice/proj/releases/{id}/assets"))
            .header(header::COOKIE, format!("session_id={}", client.session))
            .header(
                header::CONTENT_TYPE,
                "multipart/form-data; boundary=upload-test",
            )
            .body(pending())
            .unwrap()
    };
    let permits = world
        .upload_reads
        .clone()
        .acquire_many_owned(4)
        .await
        .unwrap();
    let busy = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        world.alice.router.clone().oneshot(request(&world.alice)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let denied = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        world.bob.router.clone().oneshot(request(&world.bob)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    drop(permits);
    // The read timeout is 15 minutes, so a stalled upload is cancelled by the client instead.
    let stalled = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        world.alice.router.clone().oneshot(request(&world.alice)),
    )
    .await;
    assert!(stalled.is_err());
    assert_eq!(world.upload_reads.available_permits(), 4);

    let repo = db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    for delete_release in [false, true] {
        let id = publish(
            &world.alice,
            if delete_release {
                "delete-release"
            } else {
                "delete-asset"
            },
        )
        .await;
        assert_eq!(
            upload(
                &world.alice,
                &format!("/alice/proj/releases/{id}/assets"),
                "",
                "cleanup.bin",
                b"bytes"
            )
            .await
            .status,
            StatusCode::SEE_OTHER
        );
        let assets = db::releases::list_release_assets(world.database.pool(), id)
            .await
            .unwrap();
        let asset = &assets[0];
        // Stop at the database commit, before the domain's immediate object cleanup.
        if delete_release {
            assert_eq!(
                db::releases::delete_release(
                    world.database.pool(),
                    repo.owner_user_id.unwrap(),
                    repo.id,
                    id
                )
                .await
                .unwrap()
                .into_iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
                vec![asset.file_id]
            );
        } else {
            assert_eq!(
                db::releases::delete_release_asset(
                    world.database.pool(),
                    repo.owner_user_id.unwrap(),
                    repo.id,
                    id,
                    asset.id
                )
                .await
                .unwrap()
                .unwrap()
                .id,
                asset.file_id
            );
        }
        assert!(
            db::files::get_file_by_id(world.database.pool(), asset.file_id)
                .await
                .unwrap()
                .is_none()
        );
        world
            .database
            .execute(
                "UPDATE pending_object_deletions SET delete_after='2000-01-01T00:00:00Z'",
                &[],
            )
            .await;
        assert!(!db::files::due_object_deletions(world.database.pool(), 100)
            .await
            .unwrap()
            .is_empty());
    }
    std::fs::rename(&repo.disk_path, world._root.path().join("offline.git")).unwrap();
    let reply = world
        .alice
        .form_reply(
            "/alice/proj/releases/new",
            &[
                ("repository_id", &repo.id.to_string()),
                ("tag", "outage-tag"),
                ("target", "main"),
                ("title", "Preserved title"),
                ("body", "Release notes during outage"),
            ],
        )
        .await;
    assert!(!reply.status.is_success() && !reply.status.is_redirection());
    for value in [
        "outage-tag",
        "Preserved title",
        "Release notes during outage",
    ] {
        assert!(reply.text().contains(value), "{}", reply.text());
    }
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_release_write_regressions() {
    release_write_regressions(false).await;
}
#[tokio::test]
async fn postgres_release_write_regressions() {
    release_write_regressions(true).await;
}

fn hidden_revision(html: &str) -> String {
    html.split_once("name=\"revision\" value=\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

async fn release_edit_and_read_regressions(pg: bool) {
    let mut world = setup(pg).await;
    let id = publish(&world.alice, "edits").await;
    let api = format!("/api/repos/alice/proj/releases/{id}");
    let edit = format!("/alice/proj/releases/edit/{id}");
    world
        .alice
        .expect(
            "PATCH",
            &api,
            json!({"name":"Original title", "body":"Original notes"}),
            StatusCode::OK,
        )
        .await;
    let (_, old) = world.alice.page(&edit).await;
    let revision = hidden_revision(&old);
    let preserved = world
        .alice
        .expect("PATCH", &api, json!({"prerelease":true}), StatusCode::OK)
        .await;
    assert_eq!(preserved["name"], "Original title");
    assert_eq!(preserved["body"], "Original notes");
    world
        .alice
        .expect("PATCH", &api, json!({"body":"Newer notes"}), StatusCode::OK)
        .await;
    let stale = world
        .alice
        .form_reply(
            &edit,
            &[
                ("revision", &revision),
                ("title", "My draft title"),
                ("body", "Original notes"),
                ("prerelease", "on"),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.text());
    assert!(stale.text().contains("My draft title"));
    assert!(stale.text().contains("Original notes"));
    let saved = world
        .alice
        .expect("GET", &api, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(saved["body"], "Newer notes");
    let reviewed = hidden_revision(&stale.text());
    assert_ne!(reviewed, revision);
    let confirmed = world
        .alice
        .form_reply(
            &edit,
            &[
                ("revision", &reviewed),
                ("title", "My draft title"),
                ("body", "Reviewed notes"),
            ],
        )
        .await;
    assert_eq!(
        confirmed.status,
        StatusCode::SEE_OTHER,
        "{}",
        confirmed.text()
    );
    let cleared = world
        .alice
        .expect(
            "PATCH",
            &api,
            json!({"body":null, "name":null}),
            StatusCode::OK,
        )
        .await;
    assert!(cleared["body"].is_null());
    assert!(cleared["name"].is_null());
    for name in ["a.bin", "b.bin"] {
        assert_eq!(
            upload(
                &world.alice,
                &format!("/alice/proj/releases/{id}/assets"),
                "",
                name,
                name.as_bytes()
            )
            .await
            .status,
            StatusCode::SEE_OTHER
        );
    }
    let assets = db::releases::list_release_assets(world.database.pool(), id)
        .await
        .unwrap();
    assert_eq!(
        db::releases::get_release_asset_by_name(world.database.pool(), id, "b.bin")
            .await
            .unwrap()
            .unwrap()
            .id,
        assets[1].id
    );
    assert!(
        db::releases::get_release_asset_by_name(world.database.pool(), id, "missing.bin")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db::releases::get_release_asset_by_name(world.database.pool(), -1, "a.bin")
            .await
            .unwrap()
            .is_none()
    );
    let repo = db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    std::fs::rename(&repo.disk_path, world._root.path().join("offline.git")).unwrap();
    for path in [
        "/alice/proj/releases".to_owned(),
        "/alice/proj/releases/tag/edits".to_owned(),
        edit.clone(),
    ] {
        assert_eq!(world.alice.page(&path).await.0, StatusCode::OK, "{path}");
    }
    let downloaded = world
        .bob
        .exchange(
            "GET",
            "/alice/proj/releases/download/edits/b.bin",
            "",
            Vec::new(),
        )
        .await;
    assert_eq!(downloaded.status, StatusCode::OK);
    assert_eq!(downloaded.body, b"b.bin");
    let missing = world
        .bob
        .exchange(
            "GET",
            "/alice/proj/releases/download/edits/missing.bin",
            "",
            Vec::new(),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let (_, current) = world.alice.page(&edit).await;
    let revision = hidden_revision(&current);
    assert_eq!(
        world
            .alice
            .form_reply(
                &edit,
                &[
                    ("revision", &revision),
                    ("title", "Offline edit"),
                    ("body", "Database notes")
                ]
            )
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        world
            .alice
            .form_reply(
                &format!("/alice/proj/releases/{id}/assets/{}/delete", assets[0].id),
                &[]
            )
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        world
            .alice
            .form_reply(&format!("/alice/proj/releases/{id}/delete"), &[])
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_release_edit_and_read_regressions() {
    release_edit_and_read_regressions(false).await;
}
#[tokio::test]
async fn postgres_release_edit_and_read_regressions() {
    release_edit_and_read_regressions(true).await;
}

#[tokio::test]
async fn postgres_concurrent_release_deletion_with_shared_files() {
    let mut world = setup(true).await;
    let pool = world.database.pool();
    let proj = db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let vault = db::repositories::records::get_repository(pool, "alice", "vault")
        .await
        .unwrap()
        .unwrap();
    let actor = proj.owner_user_id.unwrap();
    let first =
        db::releases::create_release(pool, actor, proj.id, "first", "main", &Default::default())
            .await
            .unwrap();
    let second =
        db::releases::create_release(pool, actor, vault.id, "second", "main", &Default::default())
            .await
            .unwrap();
    for name in ["a.bin", "b.bin"] {
        assert_eq!(
            upload(
                &world.alice,
                &format!("/alice/proj/releases/{}/assets", first.id),
                "",
                name,
                b"bytes"
            )
            .await
            .status,
            StatusCode::SEE_OTHER
        );
    }
    // Legacy rows can share uploads across repositories, in opposite asset order.
    world.database.execute("INSERT INTO release_assets(release_id,file_id,name,content_type,size) SELECT $1,file_id,name,content_type,size FROM release_assets WHERE release_id=$2 ORDER BY id DESC", &[second.id, first.id]).await;
    let (left, right) = tokio::join!(
        db::releases::delete_release(pool, actor, proj.id, first.id),
        db::releases::delete_release(pool, actor, vault.id, second.id)
    );
    let removed = left
        .unwrap()
        .into_iter()
        .chain(right.unwrap())
        .collect::<Vec<_>>();
    assert_eq!(removed.len(), 2);
    for file in removed {
        assert!(db::files::get_file_by_id(pool, file.id)
            .await
            .unwrap()
            .is_none());
    }
    world.database.finish().await;
}
