//! PR read budgets, revision checks and metadata projections on both databases.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::{database::Database, fake_webui_bundle, pull_requests::Client};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use codeyard::{
    backend::{
        api::create_router,
        app::AppState,
        ci::CiConfig,
        db,
        repositories::{self, lifecycle},
        storage::ObjectStore,
    },
    webui::{
        app::{Page, PageEnvelope},
        pages::repos::issues,
    },
};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

struct World {
    database: Database,
    _root: tempfile::TempDir,
    alice: Client,
    repository: db::models::Repository,
    alice_id: i64,
    bob_id: i64,
    base: String,
    head: String,
    pull: db::models::PullRequest,
}

impl World {
    async fn new(pg: bool) -> Self {
        fake_webui_bundle::install();
        let database = Database::new(pg).await;
        database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
        let alice_id = database.actor("alice").await;
        let bob_id = database.actor("bob").await;
        let root = tempfile::tempdir().unwrap();
        let repository = lifecycle::provision(
            (database.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
            "proj",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        db::repositories::access::grant_repo_access(
            database.pool(),
            bob_id,
            repository.id,
            db::models::AccessLevel::Write,
        )
        .await
        .unwrap();
        let mut config = CiConfig::default();
        config.trusted_actor_ids.insert(alice_id);
        let router = create_router(AppState::new(
            database.pool().clone(),
            root.path().to_owned(),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            config,
            None,
        ));
        let alice = Client::login(&router, database.pool(), alice_id).await;
        let base = alice.save("main", "README.md", "unborn").await;
        alice.branch("topic", &base).await;
        let head = alice.save("topic", "topic.txt", &base).await;
        alice.open_draft("Review", "topic", "main").await;
        let pull = db::pull_requests::records::get_pull_request(database.pool(), repository.id, 1)
            .await
            .unwrap()
            .unwrap();
        Self {
            database,
            _root: root,
            alice,
            repository,
            alice_id,
            bob_id,
            base,
            head,
            pull,
        }
    }
}

async fn bounded_model(client: &Client, path: &str) -> PageEnvelope {
    let request = Request::builder()
        .uri(path)
        .header(
            header::ACCEPT,
            codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
        )
        .header(header::COOKIE, format!("session_id={}", client.session))
        .body(Body::empty())
        .unwrap();
    let response = client.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(
        response.into_body(),
        codeyard::webui::bootstrap::MAX_PAGE_MODEL_BYTES,
    )
    .await
    .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn checks_and_counts(pg: bool) {
    let mut w = World::new(pg).await;
    db::pull_requests::records::mark_pull_request_ready(
        w.database.pool(),
        w.pull.id,
        w.alice_id,
        None,
    )
    .await
    .unwrap();
    w.alice.expect("POST", &format!("/api/repos/alice/proj/commits/{}/statuses", w.head), json!({"context":"external/check", "state":"failure", "description":"Compiler refused this change", "target_url":"https://ci.example/run/12"}), StatusCode::CREATED).await;
    let model = bounded_model(&w.alice, "/alice/proj/pulls/1/checks").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::PullChecks {
        item,
        runs,
        statuses,
        ..
    } = *model
    else {
        panic!("not checks")
    };
    assert!(runs.is_empty());
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].context, "external/check");
    assert_eq!(
        statuses[0].description.as_deref(),
        Some("Compiler refused this change")
    );
    assert_eq!(
        statuses[0].target_url.as_deref(),
        Some("https://ci.example/run/12")
    );
    assert_eq!(item.commit_count, Some(1));
    assert_eq!(item.file_count, Some(1));
    assert_eq!(item.comparison.as_ref().unwrap().failed_checks, 1);
    assert!(item.comparison.as_ref().unwrap().can_run_workflows);
    let (_, html) = w.alice.page("/alice/proj/pulls/1").await;
    assert!(html.contains("Run workflows"));
    assert!(!html.contains("Fork workflows need approval"));
    let (_, checks) = w.alice.page("/alice/proj/pulls/1/checks").await;
    assert!(checks.contains("Compiler refused this change"));
    assert!(checks.contains("https://ci.example/run/12"));
    assert!(!checks.contains("No workflow runs have started"));
    // Listing same-repository heads does not need a fork projection.
    let (status, listing) = w.alice.page("/alice/proj/pulls").await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("Review"));
    // Early merge feedback only refreshes the selected PR, not other open sources.
    w.alice.branch("unrelated", &w.head).await;
    let other = w.alice.open_pull("Other", "unrelated", "main").await;
    let other_number = other["number"].as_i64().unwrap();
    {
        let git = git2::Repository::open_bare(&w.repository.disk_path).unwrap();
        git.reference(
            "refs/heads/unrelated",
            git2::Oid::from_str(&w.base).unwrap(),
            true,
            "Move unrelated head",
        )
        .unwrap();
    }
    let refused = w
        .alice
        .form_reply("/alice/proj/pulls/1/merge", &[("confirm", "merge")])
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(w.alice.pull(other_number).await["head_sha"], w.head);
    let milestone = db::milestones::create_milestone(
        w.database.pool(),
        w.alice_id,
        w.repository.id,
        "Release",
        None,
        None,
    )
    .await
    .unwrap();
    let summary =
        db::milestones::get_milestone_by_id(w.database.pool(), w.repository.id, milestone.id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(summary.milestone.title, "Release");
    assert!(db::milestones::get_milestone_by_id(
        w.database.pool(),
        w.repository.id + 10000,
        milestone.id
    )
    .await
    .unwrap()
    .is_none());
    w.database.finish().await;
}

async fn ready_and_deleted_sources(pg: bool) {
    let mut w = World::new(pg).await;
    let expected = w.pull.updated_at.to_rfc3339();
    assert!(db::pull_requests::records::move_open_pull_request_base(
        w.database.pool(),
        w.pull.id,
        &w.base,
        &w.head,
        "main",
        None
    )
    .await
    .unwrap());
    assert!(matches!(
        db::pull_requests::records::mark_pull_request_ready(
            w.database.pool(),
            w.pull.id,
            w.alice_id,
            Some(&expected)
        )
        .await
        .unwrap(),
        db::pull_requests::records::ReadyOutcome::Changed
    ));
    let changed = db::pull_requests::records::get_pull_request_by_id(w.database.pool(), w.pull.id)
        .await
        .unwrap()
        .unwrap();
    assert!(changed.is_draft);
    assert!(matches!(
        db::pull_requests::records::mark_pull_request_ready(
            w.database.pool(),
            w.pull.id,
            w.alice_id,
            Some(&changed.updated_at.to_rfc3339())
        )
        .await
        .unwrap(),
        db::pull_requests::records::ReadyOutcome::Ready(_)
    ));
    // Deleted sources are PR-specific, not the same fork just because both ids are NULL.
    let other = db::pull_requests::records::create_pull_request(
        w.database.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: w.repository.id,
            head_repo_id: Some(w.repository.id),
            author_id: w.alice_id,
            title: "Other",
            body: "",
            head_branch: "other",
            head_sha: &w.head,
            base_branch: "main",
            base_sha: &w.base,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    w.database
        .execute(
            "UPDATE pull_requests SET head_repo_id = NULL, head_branch = 'main' WHERE repo_id = $1",
            &[w.repository.id],
        )
        .await;
    w.database
        .execute(
            "UPDATE pull_requests SET state = 'closed' WHERE id = $1",
            &[w.pull.id],
        )
        .await;
    let closed = db::pull_requests::records::get_pull_request_by_id(w.database.pool(), w.pull.id)
        .await
        .unwrap()
        .unwrap();
    let reopened = db::pull_requests::records::update_pull_request(
        w.database.pool(),
        closed.id,
        db::discussions::DiscussionEdit {
            actor_id: w.alice_id,
            expected_state: "closed",
            expected_updated: closed.updated_at,
            title: None,
            body: None,
            state: Some("open"),
        },
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(reopened.state, "open");
    assert_ne!(reopened.id, other.id);
    // Batch authorization agrees with the existing per-user projection.
    let levels = db::repositories::access::get_users_repo_access(
        w.database.pool(),
        &[w.alice_id, w.bob_id],
        w.repository.id,
    )
    .await
    .unwrap();
    for (id, level) in levels {
        assert_eq!(
            level,
            db::repositories::access::get_user_repo_access(w.database.pool(), id, w.repository.id)
                .await
                .unwrap()
        );
    }
    let owners = repositories::code_owners::resolve_owners(
        w.database.pool(),
        &w.repository,
        ["@alice", "@bob", "@bob"],
    )
    .await
    .unwrap();
    assert_eq!(owners["@alice"], [w.alice_id]);
    assert_eq!(owners["@bob"], [w.bob_id]);
    w.database.finish().await;
}

async fn review_page_budgets(pg: bool) {
    let mut w = World::new(pg).await;
    let mut review_ids = Vec::new();
    for _ in 0..2 {
        let review = db::pull_requests::reviews::add_pull_request_review(
            w.database.pool(),
            w.pull.id,
            w.bob_id,
            "commented",
            Some("Review summary"),
            Some(&w.head),
        )
        .await
        .unwrap();
        let body = if pg {
            "repeat('x', 65536)"
        } else {
            "replace(hex(zeroblob(32768)), '0', 'x')"
        };
        let sql = format!("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<200) INSERT INTO pr_comments (pull_request_id, author_id, review_id, body, path, line, side, commit_sha) SELECT $1, $2, $3, {body}, 'topic.txt', 1, 'RIGHT', '{}' FROM n", w.head);
        w.database
            .execute(&sql, &[w.pull.id, w.bob_id, review.id])
            .await;
        review_ids.push(review.id);
    }
    let mut offset = 0;
    loop {
        let page = db::pull_requests::reviews::list_review_comments_page(
            w.database.pool(),
            &review_ids,
            offset,
        )
        .await
        .unwrap();
        assert!(!page.comments.is_empty());
        assert!(
            serde_json::to_vec(
                &page
                    .comments
                    .iter()
                    .map(|entry| &entry.comment)
                    .collect::<Vec<_>>()
            )
            .unwrap()
            .len()
                < db::pull_requests::reviews::MAX_REVIEW_COMMENT_PAGE_BYTES
        );
        offset += page.comments.len() as i64;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(offset, 400, "pagination reaches every review comment");
    let model = bounded_model(&w.alice, "/alice/proj/pulls/1").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::Detail { comments, .. } = *model else {
        panic!("not detail")
    };
    let next = comments
        .reviews
        .last()
        .unwrap()
        .comments_more
        .as_ref()
        .unwrap();
    let more = bounded_model(&w.alice, next).await;
    let Page::Collaboration(more) = more.page else {
        panic!("not collaboration")
    };
    let issues::Page::Detail { comments: more, .. } = *more else {
        panic!("not detail")
    };
    assert!(more.reviews.last().unwrap().comments_start.is_some());
    // The diff's budget includes the viewer's pending comments too.
    w.database
        .execute(
            "INSERT INTO pr_reviews(pull_request_id, author_id, state) VALUES($1, $2, 'pending')",
            &[w.pull.id, w.alice_id],
        )
        .await;
    let body = if pg {
        "repeat('x', 65536)"
    } else {
        "replace(hex(zeroblob(32768)), '0', 'x')"
    };
    let sql = format!("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<200) INSERT INTO pr_comments (pull_request_id, author_id, review_id, body, path, line, side, commit_sha) SELECT $1, $2, v.id, {body}, 'topic.txt', 1, 'RIGHT', '{}' FROM n CROSS JOIN pr_reviews v WHERE v.pull_request_id=$1 AND v.author_id=$2 AND v.state='pending'", w.head);
    w.database.execute(&sql, &[w.pull.id, w.alice_id]).await;
    let model = bounded_model(&w.alice, "/alice/proj/pulls/1/diff").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::Diff {
        threads,
        truncated,
        item,
        pending_comments,
        ..
    } = *model
    else {
        panic!("not diff")
    };
    assert!(truncated, "400 valid bodies exceed the model's byte budget");
    assert!(!threads.is_empty());
    assert!(threads.len() < 400);
    assert_eq!(pending_comments, 200);
    assert_eq!(item.comparison.unwrap().head_sha, w.head);
    w.database.finish().await;
}

async fn bounded_codeowners_fallback(pg: bool) {
    let mut w = World::new(pg).await;
    let commit = {
        let repo = git2::Repository::open_bare(&w.repository.disk_path).unwrap();
        let parent = repo
            .find_commit(git2::Oid::from_str(&w.base).unwrap())
            .unwrap();
        let tree = parent.tree().unwrap();
        let mut builder = repo.treebuilder(Some(&tree)).unwrap();
        let mut text = b"/topic.txt @bob\n#".to_vec();
        text.resize(33 * 1024 * 1024, b'x');
        let blob = repo.blob(&text).unwrap();
        builder.insert("CODEOWNERS", blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("alice", "alice@example.com").unwrap();
        repo.commit(
            None,
            &signature,
            &signature,
            "Root owners",
            &tree,
            &[&parent],
        )
        .unwrap()
        .to_string()
    };
    // No .github directory: MissingPath must fall through to the bounded root read.
    let rules = repositories::code_owners::load_rules(&w.repository, None, &commit)
        .await
        .unwrap();
    assert_eq!(
        rules.owned_paths(&["topic.txt".to_owned()]).unwrap()[0].1,
        ["@bob"]
    );
    // A bad revision is not a missing file, and must not turn into empty ownership rules.
    assert!(
        repositories::code_owners::load_rules(&w.repository, None, &"f".repeat(40))
            .await
            .is_err()
    );
    w.database.finish().await;
}

#[tokio::test]
async fn sqlite_bounded_codeowners_location_fallback() {
    bounded_codeowners_fallback(false).await;
}
#[tokio::test]
async fn postgres_bounded_codeowners_location_fallback() {
    bounded_codeowners_fallback(true).await;
}

#[tokio::test]
async fn sqlite_checks_counts_and_workflow_recovery() {
    checks_and_counts(false).await;
}
#[tokio::test]
async fn postgres_checks_counts_and_workflow_recovery() {
    checks_and_counts(true).await;
}
#[tokio::test]
async fn sqlite_ready_revision_and_deleted_sources() {
    ready_and_deleted_sources(false).await;
}
#[tokio::test]
async fn postgres_ready_revision_and_deleted_sources() {
    ready_and_deleted_sources(true).await;
}
#[tokio::test]
async fn sqlite_review_comment_page_budgets() {
    review_page_budgets(false).await;
}
#[tokio::test]
async fn postgres_review_comment_page_budgets() {
    review_page_budgets(true).await;
}
