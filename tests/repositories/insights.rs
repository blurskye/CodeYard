//! Repository insights pages over HTTP: the daily activity, contributors matched to their
//! accounts, a year of code frequency, workflow usage, forks, and the read check. Each runs on
//! SQLite and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database::Database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client::Client;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use codeyard::webui::app::Page;
use codeyard::webui::pages::repos::insights::Page as Insights;
use codeyard::webui::pages::repos::page::Page as RepoPage;
use serde_json::{json, Value};
use std::sync::Arc;

async fn insights(client: &Client, path: &str) -> Insights {
    match client.model(path).await {
        (StatusCode::OK, Page::Repositories(RepoPage::Insights { page, .. })) => page,
        (status, page) => panic!("{path}: {status} {page:?}"),
    }
}

async fn insights_flow(pg: bool) {
    bundle::install();
    let database = Database::new(pg).await;
    database.complete_setup().await;
    let alice_id = database.actor("alice").await;
    let bob_id = database.actor("bob").await;
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
    let router = create_router(state);
    let alice = Client::login(&router, database.pool(), alice_id).await;
    let bob = Client::login(&router, database.pool(), bob_id).await;
    let anonymous = Client::anonymous(&router);
    let main = alice
        .save_text("main", "README.md", "one\ntwo\n", "unborn")
        .await;
    alice.save_text("main", "notes.txt", "three\n", &main).await;
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Counted", "body": ""}),
            StatusCode::CREATED,
        )
        .await;

    // Pulse counts today's commits and issue, one point per day of the timeframe.
    let Insights::Pulse { days, activity } = insights(&anonymous, "/alice/proj/insights").await
    else {
        panic!("not the pulse page");
    };
    assert_eq!((days, activity.len()), (30, 30));
    let today = activity.last().unwrap();
    assert_eq!((today.commits, today.issues_opened), (2, 1));
    let Insights::Pulse { activity, .. } =
        insights(&anonymous, "/alice/proj/insights?days=7").await
    else {
        panic!("not the pulse page");
    };
    assert_eq!(activity.len(), 7);
    let html = anonymous.page_ok("/alice/proj/insights").await;
    assert!(html.contains("href=\"/alice/proj/insights\""), "{html}");
    assert!(html.contains("Commits pushed"), "{html}");

    // The editor's commits belong to alice's account.
    let Insights::Contributors { rows } =
        insights(&anonymous, "/alice/proj/insights/contributors").await
    else {
        panic!("not the contributors page");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].username.as_deref(), rows[0].commits),
        (Some("alice"), 2)
    );

    // Three lines were added this week on main.
    let Insights::CodeFrequency { weeks } =
        insights(&anonymous, "/alice/proj/insights/code-frequency").await
    else {
        panic!("not the code frequency page");
    };
    assert_eq!(weeks.len(), 52);
    let this_week = weeks.last().unwrap();
    assert_eq!((this_week.additions, this_week.deletions), (3, 0));

    let html = anonymous.page_ok("/alice/proj/insights/actions").await;
    assert!(
        html.contains("No workflow runs in this timeframe."),
        "{html}"
    );

    // A public fork is listed for everyone.
    bob.expect(
        "POST",
        "/api/repos/alice/proj/forks",
        Value::Null,
        StatusCode::CREATED,
    )
    .await;
    let Insights::Forks {
        rows,
        page,
        has_more,
    } = insights(&anonymous, "/alice/proj/insights/forks").await
    else {
        panic!("not the forks page");
    };
    assert_eq!((page, has_more), (1, false));
    assert_eq!(
        rows.iter()
            .map(|fork| format!("{}/{}", fork.owner, fork.name))
            .collect::<Vec<_>>(),
        ["bob/proj"]
    );

    // A private repository's insights are as hidden as the repository.
    for path in [
        "/alice/vault/insights",
        "/alice/vault/insights/contributors",
        "/alice/vault/insights/code-frequency",
        "/alice/vault/insights/actions",
        "/alice/vault/insights/forks",
    ] {
        assert_eq!(
            anonymous.page(path).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
        assert_eq!(bob.page(path).await.0, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(alice.page(path).await.0, StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn sqlite_insights_flow() {
    insights_flow(false).await;
}

#[tokio::test]
async fn postgres_insights_flow() {
    insights_flow(true).await;
}
