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

#[path = "publication_regressions.rs"]
mod publication_regressions;

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

async fn list_paging_and_merged_counts(pg: bool) {
    let mut w = World::new(pg).await;
    w.database.execute("WITH RECURSIVE n(x) AS (SELECT 2 UNION ALL SELECT x+1 FROM n WHERE x<105) INSERT INTO pull_requests(repo_id,number,author_id,title,head_repo_id,head_branch,head_sha,base_branch,base_sha,created_at) SELECT p.repo_id,x,p.author_id,'Page item',p.head_repo_id,'branch-' || x,p.head_sha,p.base_branch,p.base_sha,p.created_at FROM n CROSS JOIN pull_requests p WHERE p.id=$1", &[w.pull.id]).await;
    let first = w
        .alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls?page=1&per_page=100",
            json!(null),
            StatusCode::OK,
        )
        .await;
    let second = w
        .alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls?page=2&per_page=100",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(first.as_array().unwrap().len(), 100);
    assert_eq!(second.as_array().unwrap().len(), 5);
    assert_eq!(first[0]["number"], 105);
    assert_eq!(second[4]["number"], 1);
    let (_, html) = w.alice.page("/alice/proj/pulls").await;
    assert!(html.contains("page=2"));
    let model = bounded_model(&w.alice, "/alice/proj/pulls?page=2").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::List { items, filters, .. } = *model else {
        panic!("not list")
    };
    assert_eq!(items.len(), 5);
    assert_eq!(filters.page, 2);
    assert!(!filters.has_more);
    w.database
        .execute(
            "UPDATE pull_requests SET state='merged', merged=TRUE WHERE repo_id=$1 AND number=2",
            &[w.repository.id],
        )
        .await;
    let closed = w
        .alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls?state=closed",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(closed.as_array().unwrap().len(), 1);
    assert_eq!(closed[0]["number"], 2);
    let counts = db::pull_requests::records::count_repo_pull_requests(
        w.database.pool(),
        w.repository.id,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(counts.closed, 1);
    assert_eq!(counts.all, 105);
    let model = bounded_model(&w.alice, "/alice/proj/pulls?state=closed").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::List { items, counts, .. } = *model else {
        panic!("not list")
    };
    assert_eq!(items[0].state, "merged");
    assert_eq!(counts.closed, 1);
    w.alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls?page=0",
            json!(null),
            StatusCode::BAD_REQUEST,
        )
        .await;
    w.database.finish().await;
}

async fn collaboration_metadata_and_recovery(pg: bool) {
    let mut w = World::new(pg).await;
    w.database.execute("INSERT INTO labels(repo_id,name,color,description) VALUES($1,'large catalogue','#ffffff','Only administrators need this')", &[w.repository.id]).await;
    let issue = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title":"Issue", "body":"Original"}),
            StatusCode::CREATED,
        )
        .await;
    let anonymous = Client::anonymous(&w.alice.router);
    let model = bounded_model(&anonymous, "/alice/proj/issues/2").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::Detail {
        available_labels, ..
    } = *model
    else {
        panic!("not detail")
    };
    assert!(available_labels.is_empty());
    let model = bounded_model(&w.alice, "/alice/proj/issues/2").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::Detail {
        available_labels, ..
    } = *model
    else {
        panic!("not detail")
    };
    assert_eq!(available_labels.len(), 1);
    let preview = w
        .alice
        .form_reply(
            "/alice/proj/issues/2/edit",
            &[
                ("title", "Issue"),
                ("body", "Preview"),
                ("intent", "preview"),
                ("expected_updated", issue["updated_at"].as_str().unwrap()),
                ("expected_state", "open"),
            ],
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.text());
    assert!(preview
        .text()
        .contains("name=\"return_to\" value=\"/alice/proj/issues/2#\""));
    for page in ["0", "50000000000000000", "9223372036854775807"] {
        let (status, _) = w
            .alice
            .page(&format!("/alice/proj/issues/2?event_page={page}"))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    // The author without base write access must not receive an unusable task control.
    w.database
        .execute(
            "UPDATE pull_requests SET author_id=$1, body='- [ ] Task' WHERE id=$2",
            &[w.bob_id, w.pull.id],
        )
        .await;
    w.database
        .execute(
            "DELETE FROM repo_access WHERE user_id=$1 AND repo_id=$2",
            &[w.bob_id, w.repository.id],
        )
        .await;
    let bob = Client::login(&w.alice.router, w.database.pool(), w.bob_id).await;
    let model = bounded_model(&bob, "/alice/proj/pulls/1").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::Detail {
        item,
        available_labels,
        ..
    } = *model
    else {
        panic!("not detail")
    };
    assert!(!item.can_tick);
    assert!(available_labels.is_empty());
    // Database-only collaboration must keep working without a readable Git directory.
    std::fs::rename(&w.repository.disk_path, w._root.path().join("offline.git")).unwrap();
    for path in [
        "/alice/proj/issues",
        "/alice/proj/issues/2",
        "/alice/proj/milestones",
    ] {
        let (status, body) = w.alice.page(path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let posted = w
        .alice
        .form_reply(
            "/alice/proj/issues/2/comments",
            &[("body", "Database only")],
        )
        .await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER, "{}", posted.text());
    w.database.finish().await;
}

async fn published_reviews_and_delayed_notices(pg: bool) {
    let mut w = World::new(pg).await;
    db::pull_requests::records::mark_pull_request_ready(
        w.database.pool(),
        w.pull.id,
        w.alice_id,
        None,
    )
    .await
    .unwrap();
    let bob = Client::login(&w.alice.router, w.database.pool(), w.bob_id).await;
    let vote = db::pull_requests::reviews::add_pull_request_review(
        w.database.pool(),
        w.pull.id,
        w.bob_id,
        "changes_requested",
        Some("Needs changes"),
        Some(&w.head),
    )
    .await
    .unwrap();
    let commented = db::pull_requests::reviews::add_pull_request_review(
        w.database.pool(),
        w.pull.id,
        w.bob_id,
        "commented",
        Some("Just a note"),
        Some(&w.head),
    )
    .await
    .unwrap();
    w.alice
        .expect(
            "POST",
            &format!(
                "/api/repos/alice/proj/pulls/1/reviews/{}/dismiss",
                commented.id
            ),
            json!({"reason":"Not a vote"}),
            StatusCode::NOT_FOUND,
        )
        .await;
    assert!(db::pull_requests::reviews::dismiss_pull_request_review(
        w.database.pool(),
        commented.id,
        w.alice_id,
        None
    )
    .await
    .is_err());
    let decisions = db::pull_requests::reviews::list_pull_request_review_decisions(
        w.database.pool(),
        w.pull.id,
    )
    .await
    .unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].state, "changes_requested");
    let (_, html) = w.alice.page("/alice/proj/pulls/1").await;
    assert!(html.contains(&format!("reviews/{}/dismiss", vote.id)));
    assert!(!html.contains(&format!("reviews/{}/dismiss", commented.id)));
    let root = db::pull_requests::reviews::add_pending_pull_request_comment(
        w.database.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: w.pull.id,
            author_id: w.bob_id,
            body: "Root",
            path: Some("topic.txt"),
            line: Some(1),
            side: Some("RIGHT"),
            commit_sha: Some(&w.head),
            in_reply_to_comment_id: None,
            expected_revisions: None,
        },
    )
    .await
    .unwrap();
    w.database.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<199) INSERT INTO pr_comments(pull_request_id,author_id,review_id,body) SELECT $1,$2,$3,'Pending' FROM n", &[w.pull.id,w.bob_id,root.review_id.unwrap()]).await;
    let full = bob
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/1/comments",
            json!({"body":"Full", "path":"topic.txt", "line":1, "pending":true}),
        )
        .await;
    assert!(full.0.is_client_error());
    bob.expect(
        "POST",
        "/api/repos/alice/proj/pulls/1/reviews",
        json!({"state":"commented", "commit_sha":w.head}),
        StatusCode::CREATED,
    )
    .await;
    let reply = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/comments",
            json!({"body":"Published reply", "in_reply_to_comment_id":root.id}),
            StatusCode::CREATED,
        )
        .await;
    let stored = db::pull_requests::reviews::get_pull_request_comment_by_id(
        w.database.pool(),
        reply["id"].as_i64().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stored.review_id, root.review_id);
    let comment = db::pull_requests::reviews::add_pull_request_comment(
        w.database.pool(),
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: w.pull.id,
            author_id: w.bob_id,
            body: "Deleted text",
            path: None,
            line: None,
            side: None,
            commit_sha: None,
            in_reply_to_comment_id: None,
            expected_revisions: None,
        },
    )
    .await
    .unwrap();
    assert!(db::pull_requests::reviews::delete_pull_request_comment(
        w.database.pool(),
        w.bob_id,
        w.repository.id,
        w.pull.id,
        comment.id
    )
    .await
    .unwrap());
    for (kind, key) in [
        ("pull_request_comment", "comment_id"),
        ("pull_request_comment_reply", "comment_id"),
        ("mention", "pull_request_comment_id"),
    ] {
        let delayed = db::notifications::records::create_notification(
            w.database.pool(),
            db::notifications::NewNotification {
                user_id: w.alice_id,
                actor_id: Some(w.bob_id),
                repo_id: Some(w.repository.id),
                kind,
                title: "Delayed",
                body: Some("Deleted text"),
                link: Some("/alice/proj/pulls/1"),
                metadata: &json!({key:comment.id}),
            },
        )
        .await;
        assert!(delayed
            .unwrap_err()
            .downcast_ref::<db::notifications::records::NotificationSuppressed>()
            .is_some());
    }
    w.database.finish().await;
}

async fn viewed_entries_and_base_staleness(pg: bool) {
    let mut w = World::new(pg).await;
    let head = {
        let git = git2::Repository::open_bare(&w.repository.disk_path).unwrap();
        let parent = git
            .find_commit(git2::Oid::from_str(&w.head).unwrap())
            .unwrap();
        let tree = parent.tree().unwrap();
        let mut builder = git.treebuilder(Some(&tree)).unwrap();
        builder.insert("vendor", parent.id(), 0o160000).unwrap();
        let mut directory = git.treebuilder(None).unwrap();
        directory
            .insert("child", git.blob(b"child\n").unwrap(), 0o100644)
            .unwrap();
        builder
            .insert("README.md", directory.write().unwrap(), 0o040000)
            .unwrap();
        let tree = git.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("alice", "alice@example.com").unwrap();
        git.commit(
            Some("refs/heads/topic"),
            &sig,
            &sig,
            "Gitlink and directory",
            &tree,
            &[&parent],
        )
        .unwrap()
        .to_string()
    };
    w.database
        .execute(
            &format!("UPDATE pull_requests SET head_sha='{head}' WHERE id=$1"),
            &[w.pull.id],
        )
        .await;
    for path in ["vendor", "README.md"] {
        let marked = w
            .alice
            .form_reply(
                "/alice/proj/pulls/1/viewed",
                &[
                    ("path", path),
                    ("viewed", "yes"),
                    ("expected_head", &head),
                    ("expected_base", &w.base),
                ],
            )
            .await;
        assert_eq!(
            marked.status,
            StatusCode::SEE_OTHER,
            "{path}: {}",
            marked.text()
        );
    }
    for path in ["missing".to_owned(), "x".repeat(4097)] {
        let refused = w
            .alice
            .form_reply(
                "/alice/proj/pulls/1/viewed",
                &[
                    ("path", &path),
                    ("viewed", "yes"),
                    ("expected_head", &head),
                    ("expected_base", &w.base),
                ],
            )
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{}",
            refused.text()
        );
    }
    db::pull_requests::viewed::set_pull_request_file_viewed(
        w.database.pool(),
        w.pull.id,
        w.alice_id,
        "legacy path",
        Some(&head),
    )
    .await
    .unwrap();
    let paths = vec!["vendor".to_owned(), "README.md".to_owned()];
    let marks = db::pull_requests::viewed::list_pull_request_viewed_files(
        w.database.pool(),
        w.pull.id,
        w.alice_id,
        &paths,
    )
    .await
    .unwrap();
    assert_eq!(marks.len(), 2);
    assert!(marks
        .iter()
        .any(|(path, sha)| path == "vendor" && sha == &w.head));
    // The file became a directory, so its mark follows the deleted contents.
    let deleted = git2::Oid::hash_object(git2::ObjectType::Blob, b"README.md\n")
        .unwrap()
        .to_string();
    assert!(marks
        .iter()
        .any(|(path, sha)| path == "README.md" && sha == &deleted));
    let preview = w
        .alice
        .form_reply(
            "/alice/proj/pulls/1/comments",
            &[
                ("path", "topic.txt"),
                ("line", "1"),
                ("side", "RIGHT"),
                ("commit_sha", &head),
                ("base_sha", &w.base),
                ("body", "Keep this"),
                ("intent", "preview"),
                ("from", "diff"),
            ],
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert!(preview
        .text()
        .contains("name=\"return_to\" value=\"/alice/proj/pulls/1/diff\""));
    assert!(preview
        .text()
        .contains(&format!("name=\"base_sha\" value=\"{}\"", w.base)));
    w.database
        .execute(
            &format!("UPDATE pull_requests SET base_sha='{head}' WHERE id=$1"),
            &[w.pull.id],
        )
        .await;
    let refused = w
        .alice
        .form_reply(
            "/alice/proj/pulls/1/comments",
            &[
                ("path", "topic.txt"),
                ("line", "1"),
                ("side", "LEFT"),
                ("commit_sha", &head),
                ("base_sha", &w.base),
                ("body", "Keep this"),
                ("from", "diff"),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.text());
    assert!(refused.text().contains("Keep this"));
    w.alice.expect("POST","/api/repos/alice/proj/pulls/1/comments",json!({"path":"topic.txt","line":1,"side":"LEFT","commit_sha":head,"base_sha":w.base,"body":"Stale"}),StatusCode::CONFLICT).await;
    w.database.finish().await;
}

#[tokio::test]
async fn sqlite_list_paging_and_merged_counts() {
    list_paging_and_merged_counts(false).await;
}
#[tokio::test]
async fn postgres_list_paging_and_merged_counts() {
    list_paging_and_merged_counts(true).await;
}
#[tokio::test]
async fn sqlite_collaboration_metadata_and_recovery() {
    collaboration_metadata_and_recovery(false).await;
}
#[tokio::test]
async fn postgres_collaboration_metadata_and_recovery() {
    collaboration_metadata_and_recovery(true).await;
}
#[tokio::test]
async fn sqlite_published_reviews_and_delayed_notices() {
    published_reviews_and_delayed_notices(false).await;
}
#[tokio::test]
async fn postgres_published_reviews_and_delayed_notices() {
    published_reviews_and_delayed_notices(true).await;
}
#[tokio::test]
async fn sqlite_viewed_entries_and_base_staleness() {
    viewed_entries_and_base_staleness(false).await;
}
#[tokio::test]
async fn postgres_viewed_entries_and_base_staleness() {
    viewed_entries_and_base_staleness(true).await;
}

async fn batched_codeowners_and_reviewer_authority(pg: bool) {
    let mut w = World::new(pg).await;
    w.database.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO users(username) SELECT 'member-' || x FROM n", &[]).await;
    w.database
        .execute("INSERT INTO organizations(name) VALUES('owners')", &[])
        .await;
    w.database.execute("INSERT INTO org_memberships(org_id,user_id,role) SELECT o.id,u.id,CASE WHEN u.id=$1 THEN 'owner' ELSE 'member' END FROM organizations o CROSS JOIN users u WHERE o.name='owners'", &[w.alice_id]).await;
    w.database.execute("INSERT INTO teams(org_id,name,slug) SELECT id,'Reviewers','reviewers' FROM organizations WHERE name='owners'", &[]).await;
    w.database.execute("INSERT INTO team_memberships(team_id,user_id) SELECT t.id,u.id FROM teams t CROSS JOIN users u WHERE t.slug='reviewers' AND u.username LIKE 'member-%'", &[]).await;
    w.database.execute("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=(SELECT id FROM organizations WHERE name='owners') WHERE id=$1", &[w.repository.id]).await;
    w.database.execute("INSERT INTO team_repo_access(team_id,repo_id,permission) SELECT id,$1,3 FROM teams WHERE slug='reviewers'", &[w.repository.id]).await;
    w.database
        .execute(
            "UPDATE pull_requests SET is_draft=FALSE WHERE id=$1",
            &[w.pull.id],
        )
        .await;
    w.database
        .execute(
            "UPDATE users SET is_banned=TRUE WHERE username='member-1'",
            &[],
        )
        .await;
    w.database
        .execute(
            "UPDATE users SET must_change_password=TRUE WHERE username='member-2'",
            &[],
        )
        .await;
    w.database.execute("DELETE FROM org_memberships WHERE user_id=(SELECT id FROM users WHERE username='member-3')", &[]).await;
    w.database
        .execute(
            "UPDATE users SET email='bob@example.com',email_verified=TRUE WHERE id=$1",
            &[w.bob_id],
        )
        .await;
    let repo = w.database.repository(w.repository.id).await;
    let mut owners: Vec<String> = (0..600).map(|n| format!("@missing-{n}")).collect();
    owners.extend([
        "@owners/reviewers".to_owned(),
        "@alice".to_owned(),
        "bob@example.com".to_owned(),
    ]);
    let resolved = repositories::code_owners::resolve_owners(
        w.database.pool(),
        &repo,
        owners.iter().map(String::as_str),
    )
    .await
    .unwrap();
    assert_eq!(resolved["@owners/reviewers"].len(), 997);
    assert_eq!(resolved["@alice"], [w.alice_id]);
    assert_eq!(resolved["bob@example.com"], [w.bob_id]);
    assert!(resolved["@missing-599"].is_empty());
    let pull = db::pull_requests::records::get_pull_request_by_id(w.database.pool(), w.pull.id)
        .await
        .unwrap()
        .unwrap();
    let mut ids = Vec::new();
    for names in (1..=1000)
        .map(|n| format!("member-{n}"))
        .collect::<Vec<_>>()
        .chunks(500)
    {
        ids.extend(
            db::users::accounts::get_users_by_usernames(w.database.pool(), names)
                .await
                .unwrap()
                .into_iter()
                .map(|user| user.id),
        );
    }
    ids.push(w.alice_id);
    let added = db::pull_requests::reviews::request_code_owner_reviews(
        w.database.pool(),
        &pull,
        w.alice_id,
        &ids,
    )
    .await
    .unwrap();
    assert_eq!(added.len(), 997);
    assert!(db::pull_requests::reviews::request_code_owner_reviews(
        w.database.pool(),
        &pull,
        w.alice_id,
        &ids
    )
    .await
    .unwrap()
    .is_empty());
    let oversized: Vec<String> = (0..1001).map(|n| format!("@missing-{n}")).collect();
    let error = repositories::code_owners::resolve_owners(
        w.database.pool(),
        &repo,
        oversized.iter().map(String::as_str),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("too many distinct owners"));
    w.database.finish().await;
}

#[tokio::test]
async fn sqlite_batched_codeowners_and_reviewer_authority() {
    batched_codeowners_and_reviewer_authority(false).await;
}
#[tokio::test]
async fn postgres_batched_codeowners_and_reviewer_authority() {
    batched_codeowners_and_reviewer_authority(true).await;
}

async fn replaced_repository_refuses_private_comment(pg: bool) {
    let mut w = World::new(pg).await;
    let original = w.repository.id.to_string();
    let (_, html) = w.alice.page("/alice/proj/pulls/1").await;
    assert!(html.contains(&format!("name=\"repository_id\" value=\"{original}\"")));
    w.database
        .execute(
            "UPDATE repositories SET name='renamed-private',is_private=TRUE WHERE id=$1",
            &[w.repository.id],
        )
        .await;
    lifecycle::provision(
        (w.database.pool(), None),
        w._root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(w.alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let base = w.alice.save("main", "README.md", "unborn").await;
    w.alice.branch("replacement", &base).await;
    w.alice.save("replacement", "public.txt", &base).await;
    w.alice
        .open_pull("Public replacement", "replacement", "main")
        .await;
    let reply = w
        .alice
        .form_reply(
            "/alice/proj/pulls/1/comments",
            &[
                ("body", "Private draft must not escape"),
                ("repository_id", &original),
                ("commit_sha", &w.head),
                ("intent", "comment"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.text().contains("Private draft must not escape"));
    assert!(
        reply
            .text()
            .contains(&format!("name=\"repository_id\" value=\"{original}\"")),
        "a refused draft must not silently acquire the replacement identity"
    );
    let refused = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/comments",
            json!({"body": "Private draft must not escape", "repository_id": w.repository.id}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(refused["error"], "revision_conflict");
    let comments = w
        .alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls/1/comments",
            serde_json::Value::Null,
            StatusCode::OK,
        )
        .await;
    assert!(comments.as_array().unwrap().is_empty());
    w.database.finish().await;
}

async fn stale_workflow_request_never_queues_refreshed_code(pg: bool) {
    let mut w = World::new(pg).await;
    db::pull_requests::records::mark_pull_request_ready(
        w.database.pool(),
        w.pull.id,
        w.alice_id,
        None,
    )
    .await
    .unwrap();
    let new_head = {
        let git = git2::Repository::open_bare(&w.repository.disk_path).unwrap();
        let parent = git
            .find_commit(git2::Oid::from_str(&w.head).unwrap())
            .unwrap();
        let blob = git
            .blob(b"on: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: []\n")
            .unwrap();
        let mut workflows = git.treebuilder(None).unwrap();
        workflows.insert("ci.yml", blob, 0o100644).unwrap();
        let mut github = git.treebuilder(None).unwrap();
        github
            .insert("workflows", workflows.write().unwrap(), 0o040000)
            .unwrap();
        let mut tree = git.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        tree.insert(".github", github.write().unwrap(), 0o040000)
            .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        let signature = git2::Signature::now("bob", "bob@example.invalid").unwrap();
        git.commit(
            Some("refs/heads/topic"),
            &signature,
            &signature,
            "Unrefreshed workflow",
            &tree,
            &[&parent],
        )
        .unwrap()
        .to_string()
    };
    w.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/workflows",
            json!({"expected_head": w.head}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(w.alice.pull(1).await["head_sha"], new_head);
    assert!(
        db::ci::list_ci_runs_for_repo(w.database.pool(), w.repository.id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    w.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/workflows",
            json!({"expected_head": new_head}),
            StatusCode::OK,
        )
        .await;
    let runs = db::ci::list_ci_runs_for_repo(w.database.pool(), w.repository.id, 100)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].head_sha.as_deref(), Some(new_head.as_str()));
    w.database.finish().await;
}

#[tokio::test]
async fn sqlite_replaced_repository_preserves_private_comment_draft() {
    replaced_repository_refuses_private_comment(false).await;
}
#[tokio::test]
async fn postgres_replaced_repository_preserves_private_comment_draft() {
    replaced_repository_refuses_private_comment(true).await;
}
#[tokio::test]
async fn sqlite_stale_workflow_request_does_not_queue_new_head() {
    stale_workflow_request_never_queues_refreshed_code(false).await;
}
#[tokio::test]
async fn postgres_stale_workflow_request_does_not_queue_new_head() {
    stale_workflow_request_never_queues_refreshed_code(true).await;
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
