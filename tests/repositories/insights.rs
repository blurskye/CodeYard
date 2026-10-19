//! Repository Insights over HTTP on disposable SQLite and PostgreSQL data: the pulse counts and
//! lists, the contributor, commit and code frequency figures of a seeded history, the window
//! filters, private repository access and the empty repository.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests::Client;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle::provision,
    storage::ObjectStore,
};
use codeyard::webui::app::Page;
use codeyard::webui::pages::repos::{
    insights::{HistoryNote, InsightsPage, InsightsView},
    page::Page as RepoPage,
};
use database::Database;
use serde_json::json;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;

/// One commit on `main`: its author, its author time, and the file it adds.
struct Seed {
    name: &'static str,
    email: &'static str,
    when: i64,
    file: &'static str,
    body: &'static str,
}

/// Writes `seeds` as commits on `main` of the bare repository at `path`, with the given times.
fn import_history(path: &Path, seeds: &[Seed]) {
    let mut stream = String::new();
    for seed in seeds {
        let message = format!("Add {}", seed.file);
        stream += &format!(
            "commit refs/heads/main\nauthor {0} <{1}> {2} +0000\ncommitter {0} <{1}> {2} +0000\ndata {3}\n{4}\n",
            seed.name,
            seed.email,
            seed.when,
            message.len(),
            message
        );
        stream += &format!(
            "M 100644 inline {}\ndata {}\n{}\n",
            seed.file,
            seed.body.len(),
            seed.body
        );
    }
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(path)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

async fn insights(client: &Client, query: &str) -> InsightsPage {
    let (status, page) = client.model(&format!("/alice/proj/insights{query}")).await;
    assert_eq!(status, StatusCode::OK, "{query}");
    match page {
        Page::Repositories(RepoPage::Insights { page, .. }) => page,
        other => panic!("{query}: not an Insights page: {other:?}"),
    }
}

async fn insights_contract(pg: bool) {
    // The HTML answers below render full pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let owner = || db::repositories::creation::RepositoryCreationOwner::personal(alice_id);
    let proj = provision(
        (f.pool(), None),
        root.path(),
        owner(),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let secret = provision(
        (f.pool(), None),
        root.path(),
        owner(),
        "secret",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    provision(
        (f.pool(), None),
        root.path(),
        owner(),
        "empty",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let anonymous = Client::anonymous(&router);

    // Ann's first commit is over a year old, so only the all-time figures include it.
    let now = chrono::Utc::now().timestamp();
    import_history(
        Path::new(&proj.disk_path),
        &[
            Seed {
                name: "Ann",
                email: "ann@example.invalid",
                when: now - 400 * DAY,
                file: "old.txt",
                body: "one\n",
            },
            Seed {
                name: "Bo",
                email: "bo@example.invalid",
                when: now - 2 * DAY,
                file: "b.txt",
                body: "x\ny\n",
            },
            Seed {
                name: "Ann",
                email: "ann@example.invalid",
                when: now - 2 * HOUR,
                file: "a.txt",
                body: "p\nq\nr\n",
            },
        ],
    );
    import_history(
        Path::new(&secret.disk_path),
        &[Seed {
            name: "Bo",
            email: "bo@example.invalid",
            when: now - HOUR,
            file: "s.txt",
            body: "s\n",
        }],
    );

    // A topic branch with one save, merged through a pull request, and an opened issue.
    let main_tip = git2::Repository::open_bare(&proj.disk_path)
        .unwrap()
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap()
        .to_string();
    alice.branch("topic", &main_tip).await;
    alice.save("topic", "topic.txt", &main_tip).await;
    let pull = alice.open_pull("Topic", "topic", "main").await;
    alice.merge_current(pull["number"].as_i64().unwrap()).await;
    let (status, body) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Crash on push", "body": "Body"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Pulse: the last day holds the merged pull request, the open issue and two commits by two
    // authors, adding four lines in two files.
    let InsightsView::Pulse(pulse) = insights(&alice, "?section=pulse&period=1d").await.view else {
        panic!("pulse section");
    };
    assert_eq!((pulse.merged_pulls, pulse.opened_pulls), (1, 1));
    assert_eq!((pulse.closed_issues, pulse.opened_issues), (0, 1));
    assert_eq!(pulse.merged[0].title, "Topic");
    assert_eq!(pulse.issues[0].title, "Crash on push");
    assert!(pulse.commits >= 2 && pulse.authors == 2, "{pulse:?}");
    assert_eq!((pulse.files, pulse.additions, pulse.deletions), (2, 4, 0));
    assert_eq!(pulse.history, HistoryNote::Complete);

    // The three-day window adds Bo's commit; the one-day figures are unchanged for the lines.
    let InsightsView::Pulse(three_days) = insights(&alice, "?section=pulse&period=3d").await.view
    else {
        panic!("pulse section");
    };
    assert_eq!((three_days.additions, three_days.deletions), (6, 0));
    assert_eq!(three_days.period, "3d");
    let InsightsView::Pulse(fallback) = insights(&alice, "?section=pulse&period=bogus").await.view
    else {
        panic!("pulse section");
    };
    assert_eq!(fallback.period, "1d");

    // Contributors, all time: Ann has two commits, Bo one, and the topic save belongs to Alice.
    let InsightsView::Contributors(all) = insights(&alice, "?section=contributors").await.view
    else {
        panic!("contributors section");
    };
    assert_eq!(all.rows[0].name, "Ann");
    assert_eq!(all.rows[0].commits, 2);
    assert_eq!(all.rows[0].weekly.len(), 52);
    assert_eq!(all.rows[0].weekly[51], 1);
    assert!(all
        .rows
        .iter()
        .any(|row| row.name == "Bo" && row.commits == 1));
    assert!(all.commits >= 4);
    assert_eq!(
        all.rows.iter().map(|row| row.commits).sum::<u64>(),
        all.commits
    );

    // The last year leaves out Ann's commit from more than a year ago.
    let InsightsView::Contributors(year) = insights(&alice, "?section=contributors&period=year")
        .await
        .view
    else {
        panic!("contributors section");
    };
    assert_eq!(year.period, "year");
    assert_eq!(
        year.rows
            .iter()
            .find(|row| row.name == "Ann")
            .unwrap()
            .commits,
        1
    );

    // Commits: the weekly chart holds 52 weeks, and the punch card counts every commit read.
    let InsightsView::Commits(pattern) = insights(&alice, "?section=commits").await.view else {
        panic!("commits section");
    };
    assert_eq!(pattern.weeks.len(), 52);
    assert!(pattern.weeks[51].count >= 3, "{pattern:?}");
    let punched: u64 = pattern
        .punch
        .iter()
        .flatten()
        .map(|count| u64::from(*count))
        .sum();
    assert_eq!(punched, pattern.commits);
    assert_eq!(pattern.history, HistoryNote::Complete);

    // Code frequency: the last 52 weeks hold the lines of the three commits in that span.
    let InsightsView::CodeFrequency(frequency) =
        insights(&alice, "?section=code-frequency").await.view
    else {
        panic!("code frequency section");
    };
    assert_eq!(frequency.weeks.len(), 52);
    assert_eq!((frequency.additions, frequency.deletions), (6, 0));

    // The pages render for anyone who can read the repository, with the nav.
    let (status, html) = anonymous.page("/alice/proj/insights?section=commits").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("href=\"/alice/proj/insights?section=contributors\""),
        "{html}"
    );
    assert!(html.contains("Punch card"), "{html}");

    // A private repository stays hidden from anonymous readers and shows to its owner.
    let (status, _) = anonymous.page("/alice/secret/insights").await;
    assert_ne!(status, StatusCode::OK);
    let (status, html) = alice.page("/alice/secret/insights").await;
    assert_eq!(status, StatusCode::OK, "{html}");

    // An empty repository shows its empty state, not an error.
    let (status, html) = alice.page("/alice/empty/insights").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("No commits yet"), "{html}");

    f.finish().await;
}

#[tokio::test]
async fn sqlite_repository_insights() {
    insights_contract(false).await;
}

#[tokio::test]
async fn postgres_repository_insights() {
    insights_contract(true).await;
}
