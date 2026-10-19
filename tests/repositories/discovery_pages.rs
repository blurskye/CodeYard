//! The public discovery pages on SQLite and PostgreSQL. Search answers with its result types,
//! sorts and counts, and keeps private repositories and their issues away from other people.
//! Explore narrows public repositories by sort, type and text. Stars lists the repositories the
//! viewer starred and can still read, and a profile's Stars tab lists what the viewer can read of
//! the user's stars. The help page renders for visitors and for signed-in people. Pages are read as
//! the page model the browser loads, and the counts through the search API.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::{http::StatusCode, Router};
use base64::Engine;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{self, repositories::discovery::NewForkedRepository},
    repositories::lifecycle,
    storage::ObjectStore,
};
use codeyard::webui::{
    app::Page,
    pages::{account, discovery as view, users},
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

/// A database with alice and bob, and a router that serves them. Repositories live under `root`.
struct Site {
    database: Database,
    root: tempfile::TempDir,
    router: Router,
    alice: i64,
    bob: i64,
}

async fn site(pg: bool) -> Site {
    // Discovery pages are HTML, which needs the browser bundle.
    bundle::install();
    let database = Database::new(pg).await;
    database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice = database.actor("alice").await;
    let bob = database.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    Site {
        database,
        root,
        router,
        alice,
        bob,
    }
}

/// Provisions a repository of `owner` on disk.
async fn repo(
    site: &Site,
    owner: i64,
    name: &str,
    description: &str,
    private: bool,
) -> db::models::Repository {
    lifecycle::provision(
        (site.database.pool(), None),
        site.root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        name,
        Some(description),
        private,
        None,
    )
    .await
    .unwrap()
}

/// Sets the counts that Search and Explore sort by. The numbers are written into the SQL, because
/// the fixture binds only ids.
async fn set_counts(site: &Site, repo: i64, stars: i64, forks: i64) {
    site.database
        .execute(
            &format!(
                "UPDATE repositories SET stars_count={stars}, forks_count={forks} WHERE id=$1"
            ),
            &[repo],
        )
        .await;
}

/// The page at `path`, which has to answer 200 with a discovery page.
async fn discovery(client: &Client, path: &str) -> view::Page {
    let (status, page) = client.model(path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    match page {
        Page::Discovery(page) => page,
        other => panic!("{path}: not a discovery page: {other:?}"),
    }
}

async fn search(client: &Client, path: &str) -> view::SearchModel {
    match discovery(client, path).await {
        view::Page::Search(model) => model,
        other => panic!("{path}: not a search page: {other:?}"),
    }
}

async fn explore(client: &Client, path: &str) -> view::ExploreModel {
    match discovery(client, path).await {
        view::Page::Explore(model) => model,
        other => panic!("{path}: not an explore page: {other:?}"),
    }
}

async fn stars(client: &Client) -> Vec<view::RepositoryCard> {
    stars_at(client, "/stars").await
}

/// The cards of the Stars list at `path`, with its sort and filter.
async fn stars_at(client: &Client, path: &str) -> Vec<view::RepositoryCard> {
    match discovery(client, path).await {
        view::Page::Stars(model) => model.repositories,
        other => panic!("{path}: not the stars page: {other:?}"),
    }
}

/// The Stars tab of the profile at `path`, as the profile page carries it.
async fn profile_stars(client: &Client, path: &str) -> users::ProfileStars {
    let (status, page) = client.model(path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    match page {
        Page::Identity(account::Page::Profile { stars, .. }) => stars,
        other => panic!("{path}: not a profile page: {other:?}"),
    }
}

/// "owner/name" for each card, in the order given.
fn full_names(cards: &[view::RepositoryCard]) -> Vec<String> {
    cards
        .iter()
        .map(|card| format!("{}/{}", card.owner, card.name))
        .collect()
}

fn titles(issues: &[view::IssueCard]) -> Vec<String> {
    issues.iter().map(|issue| issue.title.clone()).collect()
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

/// Creates `notes.md` as the first commit of alice's repository `repo`, through the file API.
async fn first_commit(alice: &Client, repo: &str, message: &str, content: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    alice
        .expect(
            "POST",
            &format!("/api/repos/alice/{repo}/contents/notes.md"),
            json!({"branch": "main", "content": encoded, "message": message, "expected_revision": "unborn"}),
            StatusCode::CREATED,
        )
        .await;
}

async fn search_contract(pg: bool) {
    let mut site = site(pg).await;
    let rustacean = repo(
        &site,
        site.alice,
        "rustacean",
        "Rust tools for the forge",
        false,
    )
    .await;
    repo(&site, site.alice, "gopher", "Go services", false).await;
    let secret = repo(
        &site,
        site.alice,
        "secret-rust",
        "Rust notes nobody else reads",
        true,
    )
    .await;
    let rust_cli = repo(&site, site.bob, "rust-cli", "Command line rust", false).await;
    // Issue times are read to the second in the results, so the two public ones are a second apart.
    db::issues::create_issue(
        site.database.pool(),
        rustacean.id,
        site.alice,
        "Rust borrow checker question",
        "",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    db::issues::create_issue(
        site.database.pool(),
        rust_cli.id,
        site.bob,
        "Rust in production",
        "",
    )
    .await
    .unwrap();
    db::issues::create_issue(
        site.database.pool(),
        secret.id,
        site.alice,
        "Rust secret plan",
        "",
    )
    .await
    .unwrap();
    db::users::accounts::create_user(site.database.pool(), "zebra_fan", None)
        .await
        .unwrap();
    let alice = Client::login(&site.router, site.database.pool(), site.alice).await;
    let bob = Client::login(&site.router, site.database.pool(), site.bob).await;
    let anonymous = Client::anonymous(&site.router);

    // Each type lists what matches. A private repository and its issues are listed for its owner
    // and stay out for everyone else.
    let everything = search(&alice, "/search?q=rust").await;
    assert_eq!(everything.tab, "repos");
    assert_eq!(
        sorted(full_names(&everything.repositories)),
        ["alice/rustacean", "alice/secret-rust", "bob/rust-cli"]
    );
    // Only the open type lists its results; the others report their counts.
    let issues = search(&alice, "/search?q=rust&type=issues").await;
    assert_eq!(
        sorted(titles(&issues.issues)),
        [
            "Rust borrow checker question",
            "Rust in production",
            "Rust secret plan"
        ]
    );
    assert!(everything.users.is_empty());
    for viewer in [&anonymous, &bob] {
        let seen = search(viewer, "/search?q=rust").await;
        assert_eq!(
            sorted(full_names(&seen.repositories)),
            ["alice/rustacean", "bob/rust-cli"]
        );
        let seen_issues = search(viewer, "/search?q=rust&type=issues").await;
        assert_eq!(seen_issues.issues.len(), 2);
        assert!(!titles(&seen_issues.issues)
            .iter()
            .any(|title| title.contains("secret")));
    }

    // The counts at the search API are the lengths of the lists.
    let (status, counts) = alice.api("GET", "/api/search?q=rust", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(counts["repos_count"], json!(3));
    assert_eq!(counts["issues_count"], json!(3));
    assert_eq!(counts["users_count"], json!(0));
    assert_eq!(counts["repos"].as_array().unwrap().len(), 3);
    assert_eq!(counts["issues"].as_array().unwrap().len(), 3);
    assert_eq!(counts["users"].as_array().unwrap().len(), 0);

    // Without a type the page opens on the first kind with results; a named type opens that one.
    assert_eq!(search(&alice, "/search?q=zebra").await.tab, "users");
    assert_eq!(search(&alice, "/search?q=borrow").await.tab, "issues");
    assert_eq!(
        search(&alice, "/search?q=rust&type=users").await.tab,
        "users"
    );
    assert_eq!(
        search(&alice, "/search?q=rust&type=issues").await.tab,
        "issues"
    );
    assert_eq!(
        search(&alice, "/search?q=rust&type=commits").await.tab,
        "commits"
    );
    assert_eq!(search(&alice, "/search?q=rust&type=code").await.tab, "code");
    for path in ["/search?q=rust&type=all", "/search?q=rust&sort=best"] {
        let (status, _) = alice.page(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }

    // Stars and forks order the repositories; newest and oldest order the issues. bob sees the same
    // two public repositories whatever alice's private one adds to her own view.
    set_counts(&site, rustacean.id, 10, 1).await;
    set_counts(&site, rust_cli.id, 3, 7).await;
    assert_eq!(
        full_names(&search(&bob, "/search?q=rust&sort=stars").await.repositories),
        ["alice/rustacean", "bob/rust-cli"]
    );
    assert_eq!(
        full_names(&search(&bob, "/search?q=rust&sort=forks").await.repositories),
        ["bob/rust-cli", "alice/rustacean"]
    );
    // Recently updated compares days, so both repositories match its order.
    assert_eq!(
        sorted(full_names(
            &search(&bob, "/search?q=rust&sort=updated")
                .await
                .repositories
        )),
        ["alice/rustacean", "bob/rust-cli"]
    );
    assert_eq!(
        titles(
            &search(&bob, "/search?q=rust&type=issues&sort=newest")
                .await
                .issues
        ),
        ["Rust in production", "Rust borrow checker question"]
    );
    assert_eq!(
        titles(
            &search(&bob, "/search?q=rust&type=issues&sort=oldest")
                .await
                .issues
        ),
        ["Rust borrow checker question", "Rust in production"]
    );
    site.database.finish().await;
}

#[tokio::test]
async fn sqlite_search_buckets_and_sorts() {
    search_contract(false).await;
}
#[tokio::test]
async fn postgres_search_buckets_and_sorts() {
    search_contract(true).await;
}

/// Code and commits are searched in public repositories only, on the default branch.
async fn commits_and_code_contract(pg: bool) {
    let mut site = site(pg).await;
    repo(&site, site.alice, "rustacean", "Rust tools", false).await;
    repo(&site, site.alice, "secret-rust", "Rust notes", true).await;
    let alice = Client::login(&site.router, site.database.pool(), site.alice).await;
    let anonymous = Client::anonymous(&site.router);
    first_commit(
        &alice,
        "rustacean",
        "Add zq-marker notes",
        "zq-marker public\n",
    )
    .await;
    first_commit(
        &alice,
        "secret-rust",
        "Add zq-marker secret notes",
        "zq-marker private\n",
    )
    .await;

    let code = search(&anonymous, "/search?q=zq-marker&type=code").await;
    assert_eq!(code.tab, "code");
    let hits: Vec<String> = code
        .code
        .iter()
        .map(|hit| format!("{}/{}:{}", hit.owner, hit.repo, hit.path))
        .collect();
    assert_eq!(hits, ["alice/rustacean:notes.md"]);

    let commits = search(&anonymous, "/search?q=zq-marker&type=commits").await;
    assert_eq!(commits.tab, "commits");
    let summaries: Vec<String> = commits
        .commits
        .iter()
        .map(|hit| format!("{}/{}: {}", hit.owner, hit.repo, hit.summary))
        .collect();
    assert_eq!(summaries, ["alice/rustacean: Add zq-marker notes"]);
    site.database.finish().await;
}

#[tokio::test]
async fn sqlite_search_commits_and_code_stay_in_public_repositories() {
    commits_and_code_contract(false).await;
}
#[tokio::test]
async fn postgres_search_commits_and_code_stay_in_public_repositories() {
    commits_and_code_contract(true).await;
}

async fn explore_contract(pg: bool) {
    let mut site = site(pg).await;
    let rustacean = repo(&site, site.alice, "rustacean", "Rust tools", false).await;
    let gopher = repo(&site, site.alice, "gopher", "Go services", false).await;
    let retired = repo(&site, site.alice, "old-project", "Retired work", false).await;
    repo(&site, site.alice, "secret", "Rust secret notes", true).await;
    site.database
        .execute(
            "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
            &[retired.id],
        )
        .await;
    set_counts(&site, rustacean.id, 10, 1).await;
    set_counts(&site, gopher.id, 3, 5).await;
    // Bob's fork is created last, so it is the newest public repository.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let disk_path = site
        .root
        .path()
        .join("bob-rustacean.git")
        .display()
        .to_string();
    db::repositories::discovery::create_forked_repository(
        site.database.pool(),
        NewForkedRepository {
            owner_id: site.bob,
            name: "rustacean",
            description: Some("Rust tools"),
            is_private: false,
            disk_path: &disk_path,
            forked_from: rustacean.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    let visitor = Client::anonymous(&site.router);

    // Newest first by default. Private repositories are not listed.
    let all = explore(&visitor, "/explore").await;
    assert_eq!(all.sort, "recent");
    assert_eq!(
        sorted(full_names(&all.repositories)),
        [
            "alice/gopher",
            "alice/old-project",
            "alice/rustacean",
            "bob/rustacean"
        ]
    );
    assert_eq!(full_names(&all.repositories)[0], "bob/rustacean");

    // Stars and forks put the largest count first.
    let by_stars = full_names(&explore(&visitor, "/explore?sort=stars").await.repositories);
    assert_eq!(by_stars[0], "alice/rustacean");
    assert_eq!(by_stars[1], "alice/gopher");
    let by_forks = full_names(&explore(&visitor, "/explore?sort=forks").await.repositories);
    assert_eq!(by_forks[0], "alice/gopher");
    assert_eq!(by_forks[1], "alice/rustacean");
    let updated = explore(&visitor, "/explore?sort=updated").await;
    assert_eq!(updated.sort, "updated");
    assert_eq!(updated.repositories.len(), 4);

    // Type keeps sources, forks or archived repositories only.
    let sources = explore(&visitor, "/explore?type=sources").await;
    assert_eq!(sources.kind, "sources");
    assert_eq!(
        sorted(full_names(&sources.repositories)),
        ["alice/gopher", "alice/old-project", "alice/rustacean"]
    );
    assert_eq!(
        full_names(&explore(&visitor, "/explore?type=forks").await.repositories),
        ["bob/rustacean"]
    );
    assert_eq!(
        full_names(
            &explore(&visitor, "/explore?type=archived")
                .await
                .repositories
        ),
        ["alice/old-project"]
    );

    // Text matches names and descriptions, ignoring case, and combines with the type.
    let text = explore(&visitor, "/explore?q=RUST").await;
    assert_eq!(text.query, "RUST");
    assert_eq!(
        sorted(full_names(&text.repositories)),
        ["alice/rustacean", "bob/rustacean"]
    );
    assert_eq!(
        full_names(
            &explore(&visitor, "/explore?q=rust&type=sources")
                .await
                .repositories
        ),
        ["alice/rustacean"]
    );
    assert!(explore(&visitor, "/explore?q=secret")
        .await
        .repositories
        .is_empty());

    // One page holds everything here, so a later page is empty and has nothing older.
    let first = explore(&visitor, "/explore").await;
    assert_eq!((first.page, first.has_older), (1, false));
    let later = explore(&visitor, "/explore?page=2").await;
    assert_eq!((later.page, later.has_older), (2, false));
    assert!(later.repositories.is_empty());

    for path in [
        "/explore?sort=oldest",
        "/explore?type=mirrors",
        "/explore?page=0",
        "/explore?page=abc",
    ] {
        let (status, _) = visitor.page(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    let long = "x".repeat(257);
    let (status, _) = visitor.page(&format!("/explore?q={long}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    site.database.finish().await;
}

#[tokio::test]
async fn sqlite_explore_sorts_types_and_searches() {
    explore_contract(false).await;
}
#[tokio::test]
async fn postgres_explore_sorts_types_and_searches() {
    explore_contract(true).await;
}

async fn stars_contract(pg: bool) {
    let mut site = site(pg).await;
    repo(&site, site.alice, "rustacean", "Rust tools", false).await;
    repo(&site, site.alice, "gopher", "Go services", false).await;
    repo(&site, site.alice, "private-notes", "Only mine", true).await;
    let alice = Client::login(&site.router, site.database.pool(), site.alice).await;
    let bob = Client::login(&site.router, site.database.pool(), site.bob).await;
    let anonymous = Client::anonymous(&site.router);

    // Stars belong to a signed-in person, so a visitor is sent to sign in.
    let reply = anonymous
        .exchange("GET", "/stars", "text/html", Vec::new())
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/login"));

    let (status, html) = bob.page("/stars").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("No starred repositories yet"), "{html}");
    assert!(html.contains("href=\"/explore\""), "{html}");

    // The latest star comes first. bob cannot read alice's private repository, so it stays off his
    // list, though alice can read her own.
    bob.expect(
        "PUT",
        "/api/repos/alice/rustacean/star",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let starred = bob
        .expect(
            "PUT",
            "/api/repos/alice/gopher/star",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(starred["stars_count"], json!(1));
    alice
        .expect(
            "PUT",
            "/api/repos/alice/rustacean/star",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    alice
        .expect(
            "PUT",
            "/api/repos/alice/private-notes/star",
            Value::Null,
            StatusCode::OK,
        )
        .await;

    // Each card shows the repository's count, which includes alice's star.
    let bobs = stars(&bob).await;
    assert_eq!(full_names(&bobs), ["alice/gopher", "alice/rustacean"]);
    assert_eq!(
        bobs.iter().map(|card| card.stars).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        sorted(full_names(&stars(&alice).await)),
        ["alice/private-notes", "alice/rustacean"]
    );

    // Sort and the filter narrow the same list. Most stars puts rustacean, with two stars, first.
    assert_eq!(
        full_names(&stars_at(&bob, "/stars?sort=name").await),
        ["alice/gopher", "alice/rustacean"]
    );
    assert_eq!(
        full_names(&stars_at(&bob, "/stars?sort=stars").await),
        ["alice/rustacean", "alice/gopher"]
    );
    assert_eq!(
        full_names(&stars_at(&bob, "/stars?q=rust").await),
        ["alice/rustacean"]
    );
    let (status, html) = bob.page("/stars?q=no-such-repository").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("No starred repositories match"), "{html}");

    // A profile's Stars tab counts the stars the viewer can read. bob cannot see alice's private
    // star, and anonymous visitors see only the public one.
    let own = profile_stars(&alice, "/alice?tab=stars").await;
    assert!(own.open);
    assert_eq!(own.count, 2);
    assert_eq!(
        sorted(full_names(&own.repositories)),
        ["alice/private-notes", "alice/rustacean"]
    );
    for viewer in [&bob, &anonymous] {
        let seen = profile_stars(viewer, "/alice?tab=stars").await;
        assert_eq!(seen.count, 1);
        assert_eq!(full_names(&seen.repositories), ["alice/rustacean"]);
    }
    // The count is on the repositories tab as well, without the list.
    let closed = profile_stars(&bob, "/alice").await;
    assert_eq!((closed.count, closed.open), (1, false));
    assert!(closed.repositories.is_empty());
    assert_eq!(profile_stars(&bob, "/bob?tab=stars").await.count, 2);

    bob.expect(
        "DELETE",
        "/api/repos/alice/rustacean/star",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(full_names(&stars(&bob).await), ["alice/gopher"]);
    site.database.finish().await;
}

/// The Stars list unstars with its own native form, which returns to the list it was sent from. A
/// return address that is not the list is ignored, so the action lands on the repository.
async fn stars_unstar_contract(pg: bool) {
    let mut site = site(pg).await;
    repo(&site, site.alice, "rustacean", "Rust tools", false).await;
    let bob = Client::login(&site.router, site.database.pool(), site.bob).await;
    bob.expect(
        "PUT",
        "/api/repos/alice/rustacean/star",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let (status, html) = bob.page("/stars?sort=stars").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("action=\"/alice/rustacean/_forms/unstar\""),
        "{html}"
    );
    assert!(html.contains("Unstar alice/rustacean"), "{html}");

    let reply = bob
        .form_reply(
            "/alice/rustacean/_forms/unstar",
            &[("return_to", "/stars?sort=stars")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/stars?sort=stars"));
    assert!(stars(&bob).await.is_empty());

    bob.expect(
        "PUT",
        "/api/repos/alice/rustacean/star",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let reply = bob
        .form_reply(
            "/alice/rustacean/_forms/unstar",
            &[("return_to", "https://evil.example/stars")],
        )
        .await;
    assert_eq!(reply.location.as_deref(), Some("/alice/rustacean"));
    assert!(stars(&bob).await.is_empty());
    site.database.finish().await;
}

/// A private repository is on a viewer's Stars list only while the viewer can read it. Revoking
/// access hides the star without deleting it, so granting access again brings it back.
async fn stars_access_contract(pg: bool) {
    let mut site = site(pg).await;
    repo(&site, site.alice, "private-notes", "Only mine", true).await;
    let alice = Client::login(&site.router, site.database.pool(), site.alice).await;
    let bob = Client::login(&site.router, site.database.pool(), site.bob).await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/private-notes/collaborators",
            json!({"username": "bob", "access_level": "read"}),
            StatusCode::OK,
        )
        .await;
    bob.expect(
        "PUT",
        "/api/repos/alice/private-notes/star",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(full_names(&stars(&bob).await), ["alice/private-notes"]);
    assert_eq!(profile_stars(&bob, "/bob?tab=stars").await.count, 1);

    alice
        .expect(
            "DELETE",
            "/api/repos/alice/private-notes/collaborators/bob",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    assert!(stars(&bob).await.is_empty());
    let profile = profile_stars(&bob, "/bob?tab=stars").await;
    assert_eq!(profile.count, 0);
    assert!(profile.repositories.is_empty());

    alice
        .expect(
            "POST",
            "/api/repos/alice/private-notes/collaborators",
            json!({"username": "bob", "access_level": "read"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(full_names(&stars(&bob).await), ["alice/private-notes"]);
    site.database.finish().await;
}

/// The Stars list pages through every starred repository the viewer can read, fifty to a page.
async fn stars_paging_contract(pg: bool) {
    let mut site = site(pg).await;
    site.database
        .execute(
            "WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<51) \
             INSERT INTO repositories(owner_kind,owner_user_id,name,is_private,disk_path) \
             SELECT 'user',$1,'bulk-' || n,FALSE,'/fixture/not-executed.git' FROM numbers",
            &[site.alice],
        )
        .await;
    site.database
        .execute(
            "INSERT INTO stars(repo_id,user_id,created_at) \
             SELECT id,$1,'2024-01-01T00:00:00.000000Z' FROM repositories \
             WHERE owner_user_id=$2 AND name LIKE 'bulk-%'",
            &[site.bob, site.alice],
        )
        .await;
    let bob = Client::login(&site.router, site.database.pool(), site.bob).await;
    let (status, html) = bob.page("/stars").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("page=2"), "the older page is linked: {html}");
    let first = stars(&bob).await;
    let second = stars_at(&bob, "/stars?page=2").await;
    assert_eq!((first.len(), second.len()), (50, 1));
    let names: std::collections::HashSet<String> = full_names(&first)
        .into_iter()
        .chain(full_names(&second))
        .collect();
    assert_eq!(names.len(), 51);
    assert!(stars_at(&bob, "/stars?page=9").await.is_empty());
    site.database.finish().await;
}

/// The Stars filter finds what the repository search finds for the same text, so the two fold case
/// the same way. On SQLite that includes non-ASCII letters: `café` finds `CAFÉ`. On PostgreSQL both
/// use ILIKE, which folds non-ASCII letters only under a UTF-8 locale.
async fn stars_filter_matches_what_search_matches(pg: bool) {
    let mut site = site(pg).await;
    repo(&site, site.alice, "espresso", "CAFÉ notes", false).await;
    repo(&site, site.alice, "gopher", "Go services", false).await;
    let bob = Client::login(&site.router, site.database.pool(), site.bob).await;
    bob.expect(
        "PUT",
        "/api/repos/alice/espresso/star",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let searched = full_names(
        &search(&bob, "/search?q=caf%C3%A9&type=repos")
            .await
            .repositories,
    );
    let starred = full_names(&stars_at(&bob, "/stars?q=caf%C3%A9").await);
    assert_eq!(starred, searched);
    if !pg {
        assert_eq!(starred, vec!["alice/espresso".to_owned()]);
    }
    assert!(stars_at(&bob, "/stars?q=tea").await.is_empty());
    site.database.finish().await;
}

/// The Stars tab keeps the Repositories count, and counts only the repositories the viewer can list.
/// Its list is not read, so the tab's page model carries no repositories.
async fn profile_stars_tab_counts_listable_repositories(pg: bool) {
    let mut site = site(pg).await;
    repo(&site, site.alice, "rustacean", "Rust tools", false).await;
    repo(&site, site.alice, "private-notes", "Only mine", true).await;
    let alice = Client::login(&site.router, site.database.pool(), site.alice).await;
    let bob = Client::login(&site.router, site.database.pool(), site.bob).await;
    for (client, expected) in [(&bob, 1), (&alice, 2)] {
        let (status, page) = client.model("/alice?tab=stars").await;
        assert_eq!(status, StatusCode::OK);
        match page {
            Page::Identity(account::Page::Profile {
                repositories,
                repository_count,
                ..
            }) => {
                assert_eq!(repository_count, expected);
                assert!(repositories.is_empty());
            }
            other => panic!("not a profile page: {other:?}"),
        }
    }
    site.database.finish().await;
}

/// The help page renders for a visitor and for a signed-in person, and both reach it from the
/// phone menu. Only the signed-in person has the account menu.
async fn help_contract(pg: bool) {
    let mut site = site(pg).await;
    let alice = Client::login(&site.router, site.database.pool(), site.alice).await;
    let anonymous = Client::anonymous(&site.router);
    for (who, client, signed_in) in [("visitor", &anonymous, false), ("alice", &alice, true)] {
        let (status, html) = client.page("/help").await;
        assert_eq!(status, StatusCode::OK, "{who}");
        assert!(html.contains("Keyboard shortcuts"), "{who}: {html}");
        assert!(html.contains("Report a problem"), "{who}: {html}");
        assert_eq!(
            html.matches("href=\"/help\"").count(),
            // The navigation menu and the footer each link to it, and the account menu adds one.
            if signed_in { 3 } else { 2 },
            "{who}: {html}"
        );
        assert_eq!(
            html.contains("id=\"site-account-menu\""),
            signed_in,
            "{who}"
        );
        assert!(
            matches!(client.model("/help").await.1, Page::Help),
            "{who}: the help page loads as its model"
        );
    }
    site.database.finish().await;
}

#[tokio::test]
async fn sqlite_stars_page_lists_the_viewers_readable_stars() {
    stars_contract(false).await;
}
#[tokio::test]
async fn postgres_stars_page_lists_the_viewers_readable_stars() {
    stars_contract(true).await;
}
#[tokio::test]
async fn sqlite_stars_unstar_from_the_list_and_return_there() {
    stars_unstar_contract(false).await;
}
#[tokio::test]
async fn postgres_stars_unstar_from_the_list_and_return_there() {
    stars_unstar_contract(true).await;
}
#[tokio::test]
async fn sqlite_stars_follow_current_read_access() {
    stars_access_contract(false).await;
}
#[tokio::test]
async fn postgres_stars_follow_current_read_access() {
    stars_access_contract(true).await;
}
#[tokio::test]
async fn sqlite_stars_pages_through_every_readable_star() {
    stars_paging_contract(false).await;
}
#[tokio::test]
async fn postgres_stars_pages_through_every_readable_star() {
    stars_paging_contract(true).await;
}
#[tokio::test]
async fn sqlite_stars_filter_matches_what_search_matches() {
    stars_filter_matches_what_search_matches(false).await;
}

#[tokio::test]
async fn postgres_stars_filter_matches_what_search_matches() {
    stars_filter_matches_what_search_matches(true).await;
}

#[tokio::test]
async fn sqlite_profile_stars_tab_counts_listable_repositories() {
    profile_stars_tab_counts_listable_repositories(false).await;
}

#[tokio::test]
async fn postgres_profile_stars_tab_counts_listable_repositories() {
    profile_stars_tab_counts_listable_repositories(true).await;
}

#[tokio::test]
async fn sqlite_help_page_renders_for_visitors_and_signed_in_people() {
    help_contract(false).await;
}
#[tokio::test]
async fn postgres_help_page_renders_for_visitors_and_signed_in_people() {
    help_contract(true).await;
}
