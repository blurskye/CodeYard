//! The repository wiki through the native forms, the read-only JSON API and the database: pages,
//! revisions, stale edits, renames with redirects, wiki links and the sidebar, the permission levels,
//! private and archived repositories, the setting and the limits. Each scenario runs on SQLite and
//! PostgreSQL.
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

/// `alice/proj` is public and `alice/vault` is private; alice owns both. Bob reads `proj` only.
/// Carol writes to `proj` as a collaborator and reads nothing else.
struct World {
    database: Database,
    _root: tempfile::TempDir,
    alice: Client,
    bob: Client,
    carol: Client,
    carol_id: i64,
    anonymous: Client,
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
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/collaborators",
            json!({"username": "carol", "access_level": "write"}),
            StatusCode::OK,
        )
        .await;
    let bob = Client::login(&router, database.pool(), bob_id).await;
    let carol = Client::login(&router, database.pool(), carol_id).await;
    World {
        database,
        _root: root,
        alice,
        bob,
        carol,
        carol_id,
        anonymous: Client::anonymous(&router),
    }
}

fn is_redirect_to(reply: &pulls::Reply, suffix: &str) -> bool {
    reply.status.is_redirection()
        && reply
            .location
            .as_deref()
            .is_some_and(|location| location.ends_with(suffix))
}

async fn create(client: &Client, repo: &str, title: &str, body: &str) -> pulls::Reply {
    client
        .form_reply(
            &format!("/alice/{repo}/wiki/_new"),
            &[("title", title), ("body", body), ("message", "create")],
        )
        .await
}

async fn revision_of(client: &Client, slug: &str) -> i64 {
    let value = client
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/wiki/pages/{slug}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    value["revision"].as_i64().unwrap()
}

async fn pages_are_created_edited_and_kept_in_history(pg: bool) {
    let mut world = setup(pg).await;
    let (status, empty) = world.anonymous.page("/alice/proj/wiki").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(empty.contains("This wiki has no pages yet"), "{empty}");

    let reply = create(&world.carol, "proj", "Home", "Welcome").await;
    assert!(
        is_redirect_to(&reply, "/alice/proj/wiki/home"),
        "{}",
        reply.text()
    );
    let (status, page) = world.anonymous.page("/alice/proj/wiki/home").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Welcome"), "{page}");

    let reply = world
        .carol
        .form_reply(
            "/alice/proj/wiki/home/edit",
            &[
                ("revision", "1"),
                ("title", "Home"),
                ("body", "Welcome again"),
                ("message", "fix"),
            ],
        )
        .await;
    assert!(
        is_redirect_to(&reply, "/alice/proj/wiki/home"),
        "{}",
        reply.text()
    );
    assert_eq!(revision_of(&world.alice, "home").await, 2);

    // A second save from the same opened head is refused, and the text is kept in the form.
    let (status, page) = world
        .carol
        .form(
            "/alice/proj/wiki/home/edit",
            &[
                ("revision", "1"),
                ("title", "Home"),
                ("body", "Lost edit"),
                ("message", ""),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(page.contains("Your text is kept below"), "{page}");
    assert!(page.contains("Lost edit"), "{page}");
    assert_eq!(revision_of(&world.alice, "home").await, 2);

    let (_, history) = world.anonymous.page("/alice/proj/wiki/home/history").await;
    assert!(
        history.contains("revision 2") && history.contains("revision 1"),
        "{history}"
    );
    let (_, old) = world
        .anonymous
        .page("/alice/proj/wiki/home/revisions/1")
        .await;
    assert!(
        old.contains("This is an older revision") && old.contains("Welcome"),
        "{old}"
    );
    let (_, diff) = world
        .anonymous
        .page("/alice/proj/wiki/home/compare/1/2")
        .await;
    assert!(
        diff.contains("wiki-diff-add") && diff.contains("Welcome again"),
        "{diff}"
    );

    let (status, taken) = world
        .carol
        .form(
            "/alice/proj/wiki/_new",
            &[("title", "home"), ("body", "x"), ("message", "")],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(taken.contains("already exists"), "{taken}");
    world.database.finish().await;
}

async fn renames_keep_history_and_redirect_the_old_address(pg: bool) {
    let mut world = setup(pg).await;
    create(&world.carol, "proj", "Home", "Body").await;
    let reply = world
        .carol
        .form_reply(
            "/alice/proj/wiki/home/edit",
            &[
                ("revision", "1"),
                ("title", "Start"),
                ("body", "Body"),
                ("message", "rename"),
            ],
        )
        .await;
    assert!(
        is_redirect_to(&reply, "/alice/proj/wiki/start"),
        "{}",
        reply.text()
    );
    let moved = world
        .anonymous
        .exchange("GET", "/alice/proj/wiki/home", "text/html", Vec::new())
        .await;
    assert!(
        is_redirect_to(&moved, "/alice/proj/wiki/start"),
        "{:?}",
        moved.status
    );
    let (_, history) = world.anonymous.page("/alice/proj/wiki/start/history").await;
    assert!(
        history.contains("revision 2") && history.contains("revision 1"),
        "{history}"
    );
    world.database.finish().await;
}

async fn wiki_links_and_the_sidebar_show_on_every_page(pg: bool) {
    let mut world = setup(pg).await;
    create(&world.carol, "proj", "Install Guide", "Steps").await;
    create(&world.carol, "proj", "_Sidebar", "Sidebar notes").await;
    create(
        &world.carol,
        "proj",
        "Home",
        "See [[Install Guide]] and [[Missing Thing]].",
    )
    .await;
    let (_, home) = world.anonymous.page("/alice/proj/wiki/home").await;
    assert!(
        home.contains("href=\"/alice/proj/wiki/install-guide\""),
        "{home}"
    );
    assert!(
        home.contains("/alice/proj/wiki/_new?title=Missing%20Thing"),
        "{home}"
    );
    assert!(home.contains("Sidebar notes"), "{home}");
    let (_, steps) = world.anonymous.page("/alice/proj/wiki/install-guide").await;
    assert!(steps.contains("Sidebar notes"), "{steps}");
    world.database.finish().await;
}

async fn permissions_follow_the_repository_role(pg: bool) {
    let mut world = setup(pg).await;
    create(&world.carol, "proj", "Home", "Body").await;
    // A reader sees pages and cannot change them.
    let (status, _) = world.bob.page("/alice/proj/wiki/home").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = world
        .bob
        .form(
            "/alice/proj/wiki/_new",
            &[("title", "Nope"), ("body", "x"), ("message", "")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = world.bob.form("/alice/proj/wiki/home/delete", &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A writer edits but does not delete; the administrator deletes.
    let (status, _) = world.carol.form("/alice/proj/wiki/home/delete", &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let reply = world
        .alice
        .form_reply("/alice/proj/wiki/home/delete", &[])
        .await;
    assert!(
        is_redirect_to(&reply, "/alice/proj/wiki"),
        "{}",
        reply.text()
    );
    let (status, _) = world.anonymous.page("/alice/proj/wiki/home").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    world.database.finish().await;
}

async fn private_repositories_stay_private(pg: bool) {
    let mut world = setup(pg).await;
    let reply = create(&world.alice, "vault", "Secret", "Keep out").await;
    assert!(
        is_redirect_to(&reply, "/alice/vault/wiki/secret"),
        "{}",
        reply.text()
    );
    for client in [&world.anonymous, &world.bob, &world.carol] {
        let (status, body) = client.page("/alice/vault/wiki/secret").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(!body.contains("Keep out"));
    }
    world.database.finish().await;
}

async fn archived_repositories_are_read_only(pg: bool) {
    let mut world = setup(pg).await;
    create(&world.carol, "proj", "Home", "Body").await;
    world
        .database
        .execute(
            "UPDATE repositories SET is_archived = TRUE WHERE name = 'proj'",
            &[],
        )
        .await;
    let (status, page) = world.anonymous.page("/alice/proj/wiki/home").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let (status, _) = world
        .carol
        .form(
            "/alice/proj/wiki/home/edit",
            &[
                ("revision", "1"),
                ("title", "Home"),
                ("body", "Changed"),
                ("message", ""),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(revision_of(&world.alice, "home").await, 1);
    world.database.finish().await;
}

async fn a_disabled_wiki_answers_not_found(pg: bool) {
    let mut world = setup(pg).await;
    create(&world.carol, "proj", "Home", "Body").await;
    let (status, _) = world
        .bob
        .form("/alice/proj/wiki/_settings", &[("enabled", "0")])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let reply = world
        .alice
        .form_reply("/alice/proj/wiki/_settings", &[("enabled", "0")])
        .await;
    assert!(
        is_redirect_to(&reply, "/alice/proj/settings"),
        "{}",
        reply.text()
    );
    let (status, _) = world.anonymous.page("/alice/proj/wiki/home").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = world.anonymous.page("/alice/proj/wiki").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = world.anonymous.page("/alice/proj").await;
    assert_eq!(status, StatusCode::OK);
    world
        .alice
        .form("/alice/proj/wiki/_settings", &[("enabled", "1")])
        .await;
    let (status, _) = world.anonymous.page("/alice/proj/wiki/home").await;
    assert_eq!(status, StatusCode::OK);
    world.database.finish().await;
}

async fn the_limits_are_enforced(pg: bool) {
    let mut world = setup(pg).await;
    let long_title = "a".repeat(201);
    let (status, page) = world
        .carol
        .form(
            "/alice/proj/wiki/_new",
            &[("title", &long_title), ("body", "x"), ("message", "")],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("longer than 200 characters"), "{page}");
    let big = "a".repeat(1024 * 1024 + 1);
    let (status, page) = world
        .carol
        .form(
            "/alice/proj/wiki/_new",
            &[("title", "Big"), ("body", &big), ("message", "")],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("larger than 1 MiB"), "{page}");

    let repo = db::repositories::records::get_repository(world.database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = world.carol_id;
    for number in 0..db::wiki::PAGE_LIMIT {
        let content = db::wiki::WikiContent {
            slug: &format!("page-{number}"),
            title: &format!("Page {number}"),
            body: "",
            message: "",
        };
        db::wiki::create_page(world.database.pool(), actor, repo.id, &content)
            .await
            .unwrap();
    }
    let content = db::wiki::WikiContent {
        slug: "one-more",
        title: "One more",
        body: "",
        message: "",
    };
    let refused = db::wiki::create_page(world.database.pool(), actor, repo.id, &content)
        .await
        .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<db::wiki::WikiRefusal>(),
        Some(&db::wiki::WikiRefusal::PageLimit)
    );
    world.database.finish().await;
}

async fn the_json_api_reads_pages_and_hides_disabled_wikis(pg: bool) {
    let mut world = setup(pg).await;
    create(&world.carol, "proj", "Home", "Body").await;
    let list = world
        .anonymous
        .expect(
            "GET",
            "/api/repos/alice/proj/wiki/pages",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(list[0]["slug"], "home");
    world
        .anonymous
        .expect(
            "GET",
            "/api/repos/alice/proj/wiki/pages/missing",
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    world
        .anonymous
        .expect(
            "GET",
            "/api/repos/alice/vault/wiki/pages",
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_pages_are_created_edited_and_kept_in_history() {
    pages_are_created_edited_and_kept_in_history(false).await;
}

#[tokio::test]
async fn postgres_pages_are_created_edited_and_kept_in_history() {
    pages_are_created_edited_and_kept_in_history(true).await;
}

#[tokio::test]
async fn sqlite_renames_keep_history_and_redirect() {
    renames_keep_history_and_redirect_the_old_address(false).await;
}

#[tokio::test]
async fn postgres_renames_keep_history_and_redirect() {
    renames_keep_history_and_redirect_the_old_address(true).await;
}

#[tokio::test]
async fn sqlite_wiki_links_and_sidebar() {
    wiki_links_and_the_sidebar_show_on_every_page(false).await;
}

#[tokio::test]
async fn postgres_wiki_links_and_sidebar() {
    wiki_links_and_the_sidebar_show_on_every_page(true).await;
}

#[tokio::test]
async fn sqlite_permissions_follow_the_role() {
    permissions_follow_the_repository_role(false).await;
}

#[tokio::test]
async fn postgres_permissions_follow_the_role() {
    permissions_follow_the_repository_role(true).await;
}

#[tokio::test]
async fn sqlite_private_repositories_stay_private() {
    private_repositories_stay_private(false).await;
}

#[tokio::test]
async fn postgres_private_repositories_stay_private() {
    private_repositories_stay_private(true).await;
}

#[tokio::test]
async fn sqlite_archived_repositories_are_read_only() {
    archived_repositories_are_read_only(false).await;
}

#[tokio::test]
async fn postgres_archived_repositories_are_read_only() {
    archived_repositories_are_read_only(true).await;
}

#[tokio::test]
async fn sqlite_disabled_wiki_answers_not_found() {
    a_disabled_wiki_answers_not_found(false).await;
}

#[tokio::test]
async fn postgres_disabled_wiki_answers_not_found() {
    a_disabled_wiki_answers_not_found(true).await;
}

#[tokio::test]
async fn sqlite_limits_are_enforced() {
    the_limits_are_enforced(false).await;
}

#[tokio::test]
async fn postgres_limits_are_enforced() {
    the_limits_are_enforced(true).await;
}

#[tokio::test]
async fn sqlite_json_api_reads_pages() {
    the_json_api_reads_pages_and_hides_disabled_wikis(false).await;
}

#[tokio::test]
async fn postgres_json_api_reads_pages() {
    the_json_api_reads_pages_and_hides_disabled_wikis(true).await;
}
