//! The branch list's All, Active and Stale tabs on SQLite and PostgreSQL. A branch is stale when
//! its tip commit is older than 90 days. The tabs filter the list, and their counts are the
//! lengths of the lists they show. Tags have no tabs.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle,
    storage::ObjectStore,
};
use codeyard::webui::{app::Page, pages::repos};
use database::Database;
use pulls::Client;
use std::sync::Arc;

const DAY: i64 = 24 * 60 * 60;

/// Writes a commit on `branch` (created when it does not exist) whose committer time is
/// `seconds`. The commit is empty, so only its time and its place in history matter here.
fn commit(
    repo: &git2::Repository,
    branch: &str,
    parent: Option<git2::Oid>,
    seconds: i64,
    message: &str,
) -> git2::Oid {
    let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let when = git2::Time::new(seconds, 0);
    let author = git2::Signature::new("alice", "alice@example.test", &when).unwrap();
    let parent = parent.map(|id| repo.find_commit(id).unwrap());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &author,
        &author,
        message,
        &tree,
        &parents,
    )
    .unwrap()
}

/// The branch list at `path`: the names of its rows, its tab, its stale count and how many
/// branches the page knows.
async fn refs(client: &Client, path: &str) -> (Vec<String>, String, usize, usize) {
    let (status, page) = client.model(path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    match page {
        Page::Repositories(repos::page::Page::Refs {
            rows,
            tab,
            stale,
            branches,
            ..
        }) => (
            rows.into_iter().map(|row| row.name).collect(),
            tab,
            stale,
            branches.len(),
        ),
        other => panic!("{path}: not a branch list: {other:?}"),
    }
}

/// The page without the comments Dioxus writes before its text, so a badge's count is the text
/// that follows its tag.
fn plain(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].split_once("-->").map_or("", |(_, tail)| tail);
    }
    out.push_str(rest);
    out
}

/// The count in the badge of the first tab that links to `href`. The badge is a span with the
/// count-badge class, and Dioxus adds attributes to it after the class.
fn tab_count(html: &str, href: &str) -> usize {
    // Only the branch filter's tabs carry the repo-ref-tab class, not the repository's own tabs.
    let needle = format!("href=\"{href}\" class=\"repo-ref-tab");
    html.match_indices(&needle)
        .find_map(|(start, _)| {
            let end = html[start..].find("</a>")?;
            let link = &html[start..start + end];
            let (_, badge) = link.split_once("class=\"count-badge\"")?;
            let (_, count) = badge.split_once('>')?;
            count.split('<').next()?.trim().parse::<usize>().ok()
        })
        .unwrap_or_else(|| panic!("no counted tab links to {href}"))
}

async fn tabs_contract(pg: bool) {
    // The branch list is HTML, which needs the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let now = chrono::Utc::now().timestamp();
    let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let main = commit(&bare, "main", None, now, "Start");
    commit(&bare, "fresh-feature", Some(main), now, "Feature");
    // Thirty days is still active: staleness starts after ninety.
    commit(&bare, "fresh-two", Some(main), now - 30 * DAY, "Second");
    commit(&bare, "old-cleanup", Some(main), now - 120 * DAY, "Cleanup");
    commit(
        &bare,
        "old-experiment",
        Some(main),
        now - 200 * DAY,
        "Experiment",
    );

    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), owner).await;

    // All lists every branch, the default first and then by name.
    let (names, tab, stale, total) = refs(&alice, "/alice/proj/branches").await;
    assert_eq!((tab.as_str(), stale, total), ("all", 2, 5));
    assert_eq!(
        names,
        [
            "main",
            "fresh-feature",
            "fresh-two",
            "old-cleanup",
            "old-experiment"
        ]
    );

    let (active, tab, _, _) = refs(&alice, "/alice/proj/branches?tab=active").await;
    assert_eq!(tab, "active");
    assert_eq!(active, ["main", "fresh-feature", "fresh-two"]);

    let (stale_rows, tab, _, _) = refs(&alice, "/alice/proj/branches?tab=stale").await;
    assert_eq!(tab, "stale");
    assert_eq!(stale_rows, ["old-cleanup", "old-experiment"]);

    // A tab name that is not one of the tabs shows everything.
    let (unknown, tab, _, _) = refs(&alice, "/alice/proj/branches?tab=bogus").await;
    assert_eq!((tab.as_str(), unknown.len()), ("all", 5));

    // The tabs show the same counts the lists have.
    let (status, html) = alice.page("/alice/proj/branches").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let html = plain(&html);
    assert_eq!(tab_count(&html, "/alice/proj/branches"), 5, "{html}");
    assert_eq!(
        tab_count(&html, "/alice/proj/branches?tab=active"),
        active.len()
    );
    assert_eq!(
        tab_count(&html, "/alice/proj/branches?tab=stale"),
        stale_rows.len()
    );

    // Tags have no tabs, so the stale filter is ignored there.
    let (tags, tab, _, _) = refs(&alice, "/alice/proj/tags?tab=stale").await;
    assert_eq!(tab, "all");
    assert!(tags.is_empty());

    // Tip dates are not subject to the divergence cap, including a searched late branch.
    let old = bare
        .find_reference("refs/heads/old-cleanup")
        .unwrap()
        .target()
        .unwrap();
    for index in 0..205 {
        bare.reference(&format!("refs/heads/z-old-{index:03}"), old, false, "")
            .unwrap();
    }
    let (stale_rows, _, stale, total) =
        refs(&alice, "/alice/proj/branches?tab=stale&q=z-old-204").await;
    assert!(stale_rows.contains(&"z-old-204".to_owned()));
    assert_eq!((stale, total), (207, 210));
    let (active, _, _, _) = refs(&alice, "/alice/proj/branches?tab=active").await;
    assert!(!active.iter().any(|name| name.starts_with("z-old-")));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_branch_active_and_stale_tabs() {
    tabs_contract(false).await;
}
#[tokio::test]
async fn postgres_branch_active_and_stale_tabs() {
    tabs_contract(true).await;
}
