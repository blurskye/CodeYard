//! Pull requests follow their branches through the JSON API and the HTML merge
//! form, on SQLite and PostgreSQL: editor saves, branch changes and merges move
//! the head and base commits a pull request records, merging uses the current ones,
//! and approvals count only for the head they were given for.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/fake_webui_bundle.rs"]
mod bundle;
#[path = "support/database.rs"]
mod database;
#[path = "editor_save/fixture.rs"]
mod fixture;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;
#[path = "support/pull_requests.rs"]
mod pulls;
use axum::http::StatusCode;
use codeyard::backend::{api::create_router, app::AppState, db, storage::ObjectStore};
use database::Database;
use pulls::{moved_forward, text, Client};
use serde_json::{json, Value};
use std::sync::Arc;

async fn contract(pg: bool) {
    // The HTML answers below render full pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let carol_id = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
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
        f.pool(),
        bob_id,
        repository.id,
        db::models::repositories::AccessLevel::Write,
    )
    .await
    .unwrap();
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[carol_id])
        .await;
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        codeyard::backend::ci::CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let tip_of = |branch: &str| {
        git.find_reference(&format!("refs/heads/{branch}"))
            .unwrap()
            .target()
            .unwrap()
            .to_string()
    };

    // Ref mutation rejects noncanonical aliases before exact branch protection.
    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("guarded/stable", &main0).await;
    for pattern in ["guarded/stable", "guarded/new"] {
        alice
            .expect(
                "PUT",
                "/api/repos/alice/proj/branch-protection",
                json!({"pattern": pattern, "require_pull_request": true}),
                StatusCode::OK,
            )
            .await;
    }
    let protected_tip = tip_of("guarded/stable");
    assert!(git.find_reference("refs/heads/guarded/new").is_err());
    let refs_before = codeyard::backend::repositories::store::repo_store(&repository, None)
        .refs()
        .await
        .unwrap();
    for alias in ["guarded//new", "/guarded/new", "guarded/new."] {
        let (status, body) = alice
            .api(
                "POST",
                "/api/repos/alice/proj/branches",
                json!({"name": alias, "sha": main0}),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "JSON create {alias}: {body}"
        );
    }
    assert!(git.find_reference("refs/heads/guarded/new").is_err());
    for alias in ["guarded//new", "guarded/new."] {
        let (status, page) = alice
            .form(
                "/alice/proj/_forms/create-branch",
                &[("name", alias), ("sha", main0.as_str())],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "native create-branch {alias}: {page}"
        );
    }
    assert!(git.find_reference("refs/heads/guarded/new").is_err());

    let (status, body) = alice
        .api(
            "DELETE",
            "/api/repos/alice/proj/branches/guarded%2F%2Fstable",
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "JSON delete alias: {body}");
    for alias in ["guarded//stable", "/guarded/stable"] {
        let (status, page) = alice
            .form(
                "/alice/proj/_forms/delete-branch",
                &[("name", alias), ("confirm", "yes")],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "native delete-branch {alias}: {page}"
        );
    }
    let refs_after = codeyard::backend::repositories::store::repo_store(&repository, None)
        .refs()
        .await
        .unwrap();
    assert_eq!(refs_after, refs_before, "aliases have no ref side effects");
    assert_eq!(tip_of("guarded/stable"), protected_tip);

    alice.branch("ordinary-json", &main0).await;
    alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/branches/ordinary-json",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    let (status, page) = alice
        .form(
            "/alice/proj/_forms/create-branch",
            &[("name", "ordinary-native"), ("sha", &main0)],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (status, page) = alice
        .form(
            "/alice/proj/_forms/delete-branch",
            &[("name", "ordinary-native"), ("confirm", "yes")],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");

    // A pull request records the commits its branches are at when it is opened.
    alice.branch("feature", &main0).await;
    let f1 = alice.save("feature", "feature.txt", &main0).await;
    let opened = alice.open_pull("Feature", "feature", "main").await;
    assert_eq!(opened["number"], 1);
    assert_eq!(
        (text(&opened, "head_sha"), text(&opened, "base_sha")),
        (f1.as_str(), main0.as_str())
    );

    // Saving on the head branch moves the pull request's head and revision.
    let f2 = alice.save("feature", "more.txt", &f1).await;
    let pull = alice.pull(1).await;
    assert_eq!(
        (text(&pull, "head_sha"), text(&pull, "base_sha")),
        (f2.as_str(), main0.as_str())
    );
    assert!(moved_forward(&opened, &pull));
    let (status, page) = alice
        .send("GET", "/alice/proj/pulls/1", "text/html", String::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&f2), "the page shows the new head");
    assert!(!page.contains(&f1), "and no longer the old one");

    // Saving on the base branch moves its base.
    let main1 = alice.save("main", "landed.txt", &main0).await;
    let pull = alice.pull(1).await;
    assert_eq!(
        (text(&pull, "head_sha"), text(&pull, "base_sha")),
        (f2.as_str(), main1.as_str())
    );

    // A branch deleted behind a pull request keeps its last commit, cannot be
    // merged, and is followed again when it comes back.
    alice.branch("other", &main1).await;
    let o1 = alice.save("other", "other.txt", &main1).await;
    let second = alice.open_pull("Other", "other", "main").await;
    assert_eq!(text(&second, "head_sha"), o1);
    alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/branches/other",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    let gone = alice.pull(2).await;
    assert_eq!(
        (text(&gone, "state"), text(&gone, "head_sha")),
        ("open", o1.as_str())
    );
    let (status, refused) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/2/merge",
            json!({"expected_head": o1, "expected_base": main1}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "head_branch_missing", "{refused}");
    alice.branch("other", &main0).await;
    assert_eq!(text(&alice.pull(2).await, "head_sha"), main0);

    // A merge needs the commits the pull request records now. A client that still
    // holds the old base is told to reload, then the current ones merge.
    let (status, stale) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/1/merge",
            json!({"expected_head": f2, "expected_base": main0}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["error"], "revision_conflict", "{stale}");
    assert_eq!(tip_of("main"), main1, "nothing was merged");
    let merged = alice.merge_current(1).await;
    let merge_sha = text(&merged, "sha").to_owned();
    assert_eq!(tip_of("main"), merge_sha);
    let parents: Vec<String> = git
        .find_commit(git2::Oid::from_str(&merge_sha).unwrap())
        .unwrap()
        .parent_ids()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(parents, vec![main1.clone(), f2.clone()]);

    // The merge moved the base branch, so other open pull requests follow it. The
    // merged one keeps the commits that were merged, and no longer moves.
    assert_eq!(text(&alice.pull(2).await, "base_sha"), merge_sha);
    let after = alice.save("feature", "after-merge.txt", &f2).await;
    let merged_pull = alice.pull(1).await;
    assert_eq!(text(&merged_pull, "state"), "merged");
    assert_eq!(
        (
            text(&merged_pull, "head_sha"),
            text(&merged_pull, "base_sha")
        ),
        (f2.as_str(), main1.as_str())
    );
    assert_ne!(after, f2);

    // A closed pull request is not followed; reopening it catches up.
    alice.branch("later", &merge_sha).await;
    let l1 = alice.save("later", "later.txt", &merge_sha).await;
    let third = alice.open_pull("Later", "later", "main").await;
    let closed = alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/3",
            json!({"expected_state": "open", "expected_updated": third["updated_at"], "state": "closed"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(text(&closed, "state"), "closed");
    let l2 = alice.save("later", "later-2.txt", &l1).await;
    let still_closed = alice.pull(3).await;
    assert_eq!(
        text(&still_closed, "head_sha"),
        l1,
        "closed pull requests are left alone"
    );
    let reopened = alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/3",
            json!({"expected_state": "closed", "expected_updated": still_closed["updated_at"], "state": "open"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        (text(&reopened, "state"), text(&reopened, "head_sha")),
        ("open", l2.as_str())
    );

    // The HTML merge form works from the page's own values. One opened before the
    // pull request moved is refused and nothing is merged.
    let stale_form = alice.pull(3).await;
    let l3 = alice.save("later", "later-3.txt", &l2).await;
    let form = |pull: &Value, token: &str| {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf_token", token)
            .append_pair("expected_state", text(pull, "state"))
            .append_pair("expected_updated", text(pull, "updated_at"))
            .append_pair("expected_head", text(pull, "head_sha"))
            .append_pair("expected_base", text(pull, "base_sha"))
            .append_pair("expected_base_branch", text(pull, "base_branch"))
            .append_pair("confirm", "merge")
            .finish()
    };
    let (status, page) = alice
        .send(
            "POST",
            "/alice/proj/pulls/3/merge",
            "application/x-www-form-urlencoded",
            form(&stale_form, &alice.csrf),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{page}");
    assert!(page.contains("changed after the form was opened"), "{page}");
    assert_eq!(alice.pull(3).await["state"], "open");
    let fresh_form = alice.pull(3).await;
    assert_eq!(text(&fresh_form, "head_sha"), l3);
    let (status, page) = alice
        .send(
            "POST",
            "/alice/proj/pulls/3/merge",
            "application/x-www-form-urlencoded",
            form(&fresh_form, &alice.csrf),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(alice.pull(3).await["state"], "merged");

    // Create the branch before protecting it; direct branch mutations are forbidden
    // once protection is enabled. Approvals belong to the head they were given for.
    let release0 = tip_of("main");
    alice.branch("release", &release0).await;
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern": "release", "require_pull_request": true, "required_approving_reviews": 1}),
            StatusCode::OK,
        )
        .await;
    bob.expect(
        "DELETE",
        "/api/repos/alice/proj/branches/release",
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    assert_eq!(tip_of("release"), release0);
    alice.branch("topic", &release0).await;
    let t1 = alice.save("topic", "topic.txt", &release0).await;
    alice.open_pull("Topic", "topic", "release").await;
    let (status, malformed) = bob
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/4/reviews",
            json!({"state": "approved", "commit_sha": "not-a-sha"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{malformed}");
    let (status, unpublished) = bob
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/4/reviews",
            json!({"state": "approved", "commit_sha": "f".repeat(40)}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{unpublished}");
    assert_eq!(unpublished["error"], "revision_conflict");
    bob.expect(
        "POST",
        "/api/repos/alice/proj/pulls/4/reviews",
        json!({"state": "approved", "commit_sha": t1}),
        StatusCode::CREATED,
    )
    .await;
    let reviews = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls/4/reviews",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(reviews.as_array().unwrap().len(), 1);
    assert_eq!(reviews[0]["author"]["username"], "bob");
    assert_eq!(reviews[0]["author_id"], bob_id);
    assert_eq!(reviews[0]["commit_sha"], t1);
    assert_eq!(reviews[0]["state"], "approved");
    for private_field in [
        "email",
        "password_hash",
        "is_banned",
        "must_change_password",
    ] {
        assert!(reviews[0]["author"].get(private_field).is_none());
    }
    let t2 = alice.save("topic", "topic-2.txt", &t1).await;
    let (status, refused) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/4/merge",
            json!({"expected_head": t2, "expected_base": release0}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("approving review"),
        "{refused}"
    );
    assert_eq!(tip_of("release"), release0);
    let (status, stale_review) = bob
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/4/reviews",
            json!({"state": "approved", "commit_sha": t1}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale_review}");
    assert_eq!(stale_review["error"], "revision_conflict");
    let changes = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/4/reviews",
            json!({"state": "changes_requested", "body": "Fix the topic", "commit_sha": t2}),
            StatusCode::CREATED,
        )
        .await;
    // An omitted state is a neutral comment, not a replacement vote.
    bob.expect(
        "POST",
        "/api/repos/alice/proj/pulls/4/reviews",
        json!({"body": "Discussion continues"}),
        StatusCode::CREATED,
    )
    .await;
    let refused = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/4/merge",
            json!({"expected_head": t2, "expected_base": release0}),
            StatusCode::CONFLICT,
        )
        .await;
    assert!(
        text(&refused, "message").contains("changes_requested"),
        "{refused}"
    );
    alice
        .expect(
            "POST",
            &format!(
                "/api/repos/alice/proj/pulls/4/reviews/{}/dismiss",
                changes["id"].as_i64().unwrap()
            ),
            json!({"reason": "Addressed"}),
            StatusCode::OK,
        )
        .await;
    bob.expect(
        "POST",
        "/api/repos/alice/proj/pulls/4/reviews",
        json!({"body": "Another comment"}),
        StatusCode::CREATED,
    )
    .await;
    let refused = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/4/merge",
            json!({"expected_head": t2, "expected_base": release0}),
            StatusCode::CONFLICT,
        )
        .await;
    assert!(
        text(&refused, "message").contains("approving review"),
        "{refused}"
    );
    // A site administrator's documented personal-repository write override also
    // makes the non-author review eligible without a separate collaborator grant.
    carol
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/4/reviews",
            json!({"state": "approved", "commit_sha": t2}),
            StatusCode::CREATED,
        )
        .await;
    bob.expect(
        "POST",
        "/api/repos/alice/proj/pulls/4/reviews",
        json!({"body": "Approved with a follow-up comment"}),
        StatusCode::CREATED,
    )
    .await;
    let merged = alice.merge_current(4).await;
    assert_eq!(tip_of("release"), text(&merged, "sha"));

    drop(git);
    f.finish().await;
}

async fn concurrent_refresh_converges(pg: bool) {
    let mut f = Database::new(pg).await;
    let actor = f
        .actor(if pg {
            "pg_refresh_race"
        } else {
            "sqlite_refresh_race"
        })
        .await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "refresh-race",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        codeyard::backend::ci::CiConfig::default(),
        None,
    );
    let ci = state.ci_runs.clone();
    let store = codeyard::backend::repositories::store::repo_store(&repository, None);
    let main = store
        .save_file_as(
            f.pool(),
            repository.id,
            actor,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: "README.md".into(),
                message: "initial".into(),
                expected_revision: "unborn".into(),
                creating: true,
                deleting: false,
            },
            b"initial\n",
            "Refresh",
            "refresh@example.invalid",
        )
        .await
        .unwrap();
    store
        .create_branch_as(f.pool(), repository.id, actor, "topic", &main.sha)
        .await
        .unwrap();
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: repository.id,
            head_repo_id: Some(repository.id),
            author_id: actor,
            title: "Refresh race",
            body: "",
            head_branch: "topic",
            head_sha: &main.sha,
            base_branch: "main",
            base_sha: &main.sha,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let first_head = store
        .save_file_as(
            f.pool(),
            repository.id,
            actor,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "topic".into(),
                path: "one.txt".into(),
                message: "first head".into(),
                expected_revision: main.sha.clone(),
                creating: true,
                deleting: false,
            },
            b"one\n",
            "Refresh",
            "refresh@example.invalid",
        )
        .await
        .unwrap();
    let first_base = store
        .save_file_as(
            f.pool(),
            repository.id,
            actor,
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: "base-one.txt".into(),
                message: "first base".into(),
                expected_revision: main.sha.clone(),
                creating: true,
                deleting: false,
            },
            b"base one\n",
            "Refresh",
            "refresh@example.invalid",
        )
        .await
        .unwrap();
    let second_head = store
        .prepare_file(
            &codeyard::backend::repositories::store::FileWrite {
                branch: "topic".into(),
                path: "two.txt".into(),
                message: "second head".into(),
                expected_revision: first_head.sha.clone(),
                creating: true,
                deleting: false,
            },
            b"two\n",
            "Refresh",
            "refresh@example.invalid",
        )
        .await
        .unwrap();
    let second_base = store
        .prepare_file(
            &codeyard::backend::repositories::store::FileWrite {
                branch: "main".into(),
                path: "base-two.txt".into(),
                message: "second base".into(),
                expected_revision: first_base.sha.clone(),
                creating: true,
                deleting: false,
            },
            b"base two\n",
            "Refresh",
            "refresh@example.invalid",
        )
        .await
        .unwrap();

    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let pull_id = pull.id;
    let blocked_head = first_head.sha.clone();
    let blocked_base = first_base.sha.clone();
    let blocker = match &f {
        Database::Postgres(database) => {
            let raw = database.raw.clone();
            tokio::spawn(async move {
                let mut tx = raw.begin().await.unwrap();
                sqlx::query("SELECT id FROM pull_requests WHERE id=$1 FOR UPDATE")
                    .bind(pull_id)
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                locked_tx.send(()).unwrap();
                release_rx.await.unwrap();
                sqlx::query("UPDATE pull_requests SET head_sha=$2,base_sha=$3 WHERE id=$1")
                    .bind(pull_id)
                    .bind(blocked_head)
                    .bind(blocked_base)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            })
        }
        Database::Sqlite(database) => {
            let raw = database.raw.clone();
            tokio::spawn(async move {
                let mut tx = raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
                sqlx::query("SELECT id FROM pull_requests WHERE id=?1")
                    .bind(pull_id)
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                locked_tx.send(()).unwrap();
                release_rx.await.unwrap();
                sqlx::query("UPDATE pull_requests SET head_sha=?2,base_sha=?3 WHERE id=?1")
                    .bind(pull_id)
                    .bind(blocked_head)
                    .bind(blocked_base)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            })
        }
    };
    locked_rx.await.unwrap();
    let spawn_refresh = || {
        let pool = f.pool().clone();
        let repository = repository.clone();
        let ci = ci.clone();
        tokio::spawn(async move {
            codeyard::backend::repositories::pull_requests::refresh_open_pull_requests(
                &pool,
                &repository,
                Some(&["main", "topic"][..]),
                &ci,
                actor,
            )
            .await
        })
    };
    let first = spawn_refresh();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !first.is_finished(),
        "first refresh must be waiting on its CAS"
    );
    store
        .publish_file_as(f.pool(), repository.id, actor, &second_head)
        .await
        .unwrap();
    store
        .publish_file_as(f.pool(), repository.id, actor, &second_base)
        .await
        .unwrap();
    let second = spawn_refresh();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !second.is_finished(),
        "second refresh must be waiting on its CAS"
    );
    release_tx.send(()).unwrap();
    blocker.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
    })
    .await
    .unwrap();

    let stored = db::pull_requests::records::get_pull_request_by_id(f.pool(), pull.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.head_sha, second_head.commit.sha);
    assert_eq!(stored.base_sha, second_base.commit.sha);
    f.finish().await;
}

/// A moved branch refreshes only the open pull requests that use it. A pull request on
/// another branch keeps the commits it recorded until its own branch moves.
async fn refresh_touches_only_pull_requests_on_the_moved_branch(pg: bool) {
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        codeyard::backend::ci::CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;

    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("feature", &main0).await;
    alice.branch("other", &main0).await;
    let feature0 = alice.save("feature", "feature.txt", &main0).await;
    let other0 = alice.save("other", "other.txt", &main0).await;
    alice.open_pull("Feature", "feature", "main").await;
    alice.open_pull("Other", "other", "main").await;

    // Git moves `other` without a refresh, so pull request 2 now lags its branch.
    let parent = git
        .find_commit(git2::Oid::from_str(&other0).unwrap())
        .unwrap();
    let author = git2::Signature::now("Alice", "alice@example.invalid").unwrap();
    let other1 = git
        .commit(
            Some("refs/heads/other"),
            &author,
            &author,
            "Move other",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap()
        .to_string();
    assert_eq!(text(&alice.pull(2).await, "head_sha"), other0);

    // A save on `feature` refreshes pull request 1 and leaves pull request 2 alone.
    let feature1 = alice.save("feature", "more.txt", &feature0).await;
    assert_eq!(text(&alice.pull(1).await, "head_sha"), feature1);
    assert_eq!(
        text(&alice.pull(2).await, "head_sha"),
        other0,
        "a pull request on another branch is not refreshed"
    );

    // A change to `other` brings pull request 2 up to date.
    let other2 = alice.save("other", "other-2.txt", &other1).await;
    assert_eq!(text(&alice.pull(2).await, "head_sha"), other2);

    drop(parent);
    drop(git);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_refresh_touches_only_pull_requests_on_the_moved_branch() {
    refresh_touches_only_pull_requests_on_the_moved_branch(false).await;
}

#[tokio::test]
async fn postgres_refresh_touches_only_pull_requests_on_the_moved_branch() {
    refresh_touches_only_pull_requests_on_the_moved_branch(true).await;
}

/// A JSON merge that names the base branch the client read is refused once the pull request
/// targets another branch, even though both branches hold the same commit. Nothing is published,
/// and naming the branch the pull request now targets merges into that one.
async fn merge_names_the_base_branch_it_read(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        codeyard::backend::ci::CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let tip_of = |branch: &str| {
        git.find_reference(&format!("refs/heads/{branch}"))
            .unwrap()
            .target()
            .unwrap()
            .to_string()
    };

    let main0 = alice.save("main", "README.md", "unborn").await;
    alice.branch("develop", &main0).await;
    alice.branch("topic", &main0).await;
    alice.save("topic", "topic.txt", &main0).await;
    alice.open_pull("Topic", "topic", "main").await;
    // The client reads the pull request while it targets main.
    let read = alice.pull(1).await;
    // Another writer moves it onto develop, which holds the same commit.
    alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/pulls/1",
            json!({"expected_state": "open", "expected_updated": text(&read, "updated_at"), "base": "develop"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(text(&alice.pull(1).await, "base_branch"), "develop");

    // The stale request names main and the commits it read. The commits match, so only the
    // branch differs.
    let (status, refused) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/pulls/1/merge",
            json!({"expected_head": text(&read, "head_sha"), "expected_base": text(&read, "base_sha"), "expected_base_branch": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "revision_conflict", "{refused}");
    assert_eq!(
        (tip_of("main"), tip_of("develop")),
        (main0.clone(), main0.clone()),
        "neither branch moved"
    );
    assert_eq!(text(&alice.pull(1).await, "state"), "open");

    // The page's merge form carries the branch it shows, and naming that branch merges into it.
    let (status, html) = alice.page("/alice/proj/pulls/1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("name=\"expected_base_branch\" value=\"develop\""),
        "{html}"
    );
    let current = alice.pull(1).await;
    let merged = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/merge",
            json!({"expected_head": text(&current, "head_sha"), "expected_base": text(&current, "base_sha"), "expected_base_branch": "develop"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(tip_of("develop"), text(&merged, "sha"));
    assert_eq!(tip_of("main"), main0);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_merge_names_the_base_branch_it_read() {
    merge_names_the_base_branch_it_read(false).await;
}

#[tokio::test]
async fn postgres_merge_names_the_base_branch_it_read() {
    merge_names_the_base_branch_it_read(true).await;
}

#[tokio::test]
async fn sqlite_pull_requests_follow_their_branches() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_pull_requests_follow_their_branches() {
    contract(true).await;
}

#[tokio::test]
async fn sqlite_concurrent_refreshes_converge() {
    concurrent_refresh_converges(false).await;
}

#[tokio::test]
async fn postgres_concurrent_refreshes_converge() {
    concurrent_refresh_converges(true).await;
}
