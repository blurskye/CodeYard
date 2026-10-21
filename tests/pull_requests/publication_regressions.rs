use super::*;
use base64::Engine as _;
use codeyard::backend::{
    ci::CiRuns,
    notifications::NotificationBroadcaster,
    repositories::{pull_requests as pulls, store::repo_store},
};

const WORKFLOW: &str = "on: [pull_request, push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n";

fn ci(w: &World) -> CiRuns {
    let mut config = CiConfig::default();
    config.trusted_actor_ids.insert(w.alice_id);
    CiRuns::new(
        w.database.pool().clone(),
        Arc::new(config),
        Arc::new(tokio::sync::Notify::new()),
        None,
        Arc::new(NotificationBroadcaster::new(w.database.pool())),
    )
}

async fn save_text(w: &World, branch: &str, path: &str, text: &str, old: &str) -> String {
    let saved = w
        .alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/contents/{path}"),
            json!({"branch":branch,"message":"Add fixture", "expected_revision":old,
            "content":base64::engine::general_purpose::STANDARD.encode(text)}),
            StatusCode::CREATED,
        )
        .await;
    saved["sha"].as_str().unwrap().to_owned()
}

async fn row(w: &World, number: i32) -> db::models::PullRequest {
    db::pull_requests::records::get_pull_request(w.database.pool(), w.repository.id, number)
        .await
        .unwrap()
        .unwrap()
}

async fn metadata_failure(w: &World, pg: bool) {
    if pg {
        w.database.execute("CREATE FUNCTION refuse_metadata() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'metadata unavailable'; END; $$", &[]).await;
        w.database.execute("CREATE TRIGGER refuse_metadata BEFORE UPDATE OF default_branch ON repositories FOR EACH ROW EXECUTE FUNCTION refuse_metadata()", &[]).await;
    } else {
        w.database.execute("CREATE TRIGGER refuse_metadata BEFORE UPDATE OF default_branch ON repositories BEGIN SELECT RAISE(ABORT, 'metadata unavailable'); END", &[]).await;
    }
}

async fn removed_author(pg: bool) {
    let mut w = World::new(pg).await;
    w.database
        .execute(
            "UPDATE repositories SET is_private=TRUE WHERE id=$1",
            &[w.repository.id],
        )
        .await;
    w.database
        .execute(
            "UPDATE pull_requests SET author_id=$1 WHERE id=$2",
            &[w.bob_id, w.pull.id],
        )
        .await;
    assert!(db::repositories::access::revoke_repo_access(
        w.database.pool(),
        w.bob_id,
        w.repository.id
    )
    .await
    .unwrap());
    let head = w.alice.save("topic", "fixed.txt", &w.head).await;
    assert_eq!(row(&w, 1).await.head_sha, head);
    assert_eq!(
        repo_store(&w.repository, None)
            .commit("refs/pull/1/head")
            .await
            .unwrap()
            .sha,
        head
    );
    w.database.finish().await;
}

async fn inline_revision(pg: bool) {
    let mut w = World::new(pg).await;
    for pending in [false, true] {
        for (head, base) in [
            ("1".repeat(40), w.base.clone()),
            (w.head.clone(), "2".repeat(40)),
        ] {
            w.database
                .execute(
                    &format!(
                        "UPDATE pull_requests SET head_sha='{head}', base_sha='{base}' WHERE id=$1"
                    ),
                    &[w.pull.id],
                )
                .await;
            let input = db::pull_requests::reviews::NewPullRequestComment {
                pr_id: w.pull.id,
                author_id: w.bob_id,
                body: "stale",
                path: Some("topic.txt"),
                line: Some(1),
                side: Some("RIGHT"),
                commit_sha: Some(&w.head),
                in_reply_to_comment_id: None,
                expected_revisions: Some((&w.head, &w.base)),
            };
            let result = if pending {
                db::pull_requests::reviews::add_pending_pull_request_comment(
                    w.database.pool(),
                    input,
                )
                .await
            } else {
                db::pull_requests::reviews::add_pull_request_comment(w.database.pool(), input).await
            };
            assert!(matches!(
                result
                    .unwrap_err()
                    .downcast_ref::<db::discussions::DiscussionWriteRefusal>(),
                Some(db::discussions::DiscussionWriteRefusal::RevisionChanged)
            ));
        }
    }
    assert_eq!(
        db::pull_requests::reviews::count_pull_request_comments(w.database.pool(), w.pull.id)
            .await
            .unwrap(),
        0
    );
    assert!(
        db::pull_requests::reviews::get_pending_review(w.database.pool(), w.pull.id, w.bob_id)
            .await
            .unwrap()
            .is_none()
    );
    w.database.finish().await;
}

async fn deleted_comment_notifications(pg: bool) {
    let mut w = World::new(pg).await;
    let comment = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/comments",
            json!({"body":"hello"}),
            StatusCode::CREATED,
        )
        .await;
    let id = comment["id"].as_i64().unwrap();
    for deleted in [false, true] {
        if deleted {
            let reply = w
                .alice
                .form_reply(&format!("/alice/proj/pulls/1/comments/{id}/delete"), &[])
                .await;
            assert!(reply.status.is_redirection(), "{}", reply.text());
            assert!(db::pull_requests::reviews::get_pull_request_comment_by_id(
                w.database.pool(),
                id
            )
            .await
            .unwrap()
            .is_none());
        }
        for (kind, metadata) in [
            ("pull_request_comment", json!({"comment_id":id})),
            ("pull_request_comment_reply", json!({"comment_id":id})),
            ("mention", json!({"pull_request_comment_id":id})),
        ] {
            let result = db::notifications::records::create_notification(
                w.database.pool(),
                db::notifications::records::NewNotification {
                    user_id: w.bob_id,
                    actor_id: Some(w.alice_id),
                    repo_id: Some(w.repository.id),
                    kind,
                    title: "Comment",
                    body: Some("hello"),
                    link: Some("/dead"),
                    metadata: &metadata,
                },
            )
            .await;
            if deleted {
                assert!(result
                    .unwrap_err()
                    .is::<db::notifications::records::NotificationSuppressed>());
            } else {
                result.unwrap();
            }
        }
    }
    assert!(db::notifications::records::list_notifications_for_user(
        w.database.pool(),
        w.bob_id,
        100,
        false
    )
    .await
    .unwrap()
    .is_empty());
    w.database.finish().await;
}

async fn recovery(pg: bool) {
    let mut w = World::new(pg).await;
    let base = save_text(&w, "main", "CODEOWNERS", "* @bob\n", &w.base).await;
    let base = save_text(&w, "main", ".github/workflows/ci.yml", "on: [pull_request]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n", &base).await;
    w.alice.branch("recover", &base).await;
    let head = w.alice.save("recover", "recovery.txt", &base).await;
    let pull = db::pull_requests::records::create_pull_request(
        w.database.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: w.repository.id,
            head_repo_id: Some(w.repository.id),
            author_id: w.alice_id,
            title: "Recover",
            body: "",
            head_branch: "recover",
            head_sha: &head,
            base_branch: "main",
            base_sha: &base,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let value = w
            .alice
            .expect(
                "POST",
                "/api/repos/alice/proj/pulls",
                json!({"title":"Recover", "head_branch":"recover", "base_branch":"main"}),
                StatusCode::CONFLICT,
            )
            .await;
        assert_eq!(value["error"], "pull_request_exists");
    }
    let runs = db::ci::latest_ci_runs_for_commit(
        w.database.pool(),
        w.repository.id,
        &head,
        pull.id,
        "main",
    )
    .await
    .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].attempt, 1);
    let reviewers =
        db::pull_requests::reviews::list_pull_request_reviewers(w.database.pool(), pull.id)
            .await
            .unwrap();
    assert_eq!(
        reviewers.iter().map(|u| u.id).collect::<Vec<_>>(),
        [w.bob_id]
    );
    assert_eq!(
        repo_store(&w.repository, None)
            .commit(&format!("refs/pull/{}/head", pull.number))
            .await
            .unwrap()
            .sha,
        head
    );
    w.database.finish().await;
}

async fn stack_target(pg: bool) {
    let mut w = World::new(pg).await;
    let base = save_text(&w, "main", "CODEOWNERS", "src/ @bob\n", &w.base).await;
    let workflow = "on:\n  pull_request:\n    branches: [main]\n    paths: ['src/**']\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n";
    let base = save_text(&w, "main", ".github/workflows/ci.yml", workflow, &base).await;
    w.alice.branch("lower", &base).await;
    let lower = w.alice.save("lower", "src/code.rs", &base).await;
    w.alice.open_pull("Lower", "lower", "main").await;
    w.alice.branch("upper", &lower).await;
    let head = w.alice.save("upper", "docs.md", &lower).await;
    let upper = w.alice.open_pull("Upper", "upper", "lower").await;
    let id = upper["id"].as_i64().unwrap();
    assert!(
        db::pull_requests::reviews::list_pull_request_reviewers(w.database.pool(), id)
            .await
            .unwrap()
            .is_empty()
    );
    w.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/3/stack",
            json!({}),
            StatusCode::OK,
        )
        .await;
    let reviewers = db::pull_requests::reviews::list_pull_request_reviewers(w.database.pool(), id)
        .await
        .unwrap();
    assert!(reviewers.iter().any(|user| user.id == w.bob_id));
    let runs =
        db::ci::latest_ci_runs_for_commit(w.database.pool(), w.repository.id, &head, id, "main")
            .await
            .unwrap();
    assert_eq!(
        runs.len(),
        1,
        "path filtering must include lower-layer changes"
    );
    assert_eq!(
        runs[0].pull_request_event.as_ref().unwrap()["base_sha"],
        base
    );
    let snapshot = db::pull_requests::records::list_stack_members(
        w.database.pool(),
        w.repository.id,
        row(&w, 3).await.stack_id.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(snapshot.len(), 2);
    w.alice.branch("top", &head).await;
    let own = "on:\n  pull_request:\n    branches: [upper]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n";
    let top = save_text(&w, "top", ".github/workflows/own.yml", own, &head).await;
    // A new member joins after an earlier caller's membership read.
    let new = db::pull_requests::records::create_pull_request(
        w.database.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: w.repository.id,
            head_repo_id: Some(w.repository.id),
            author_id: w.alice_id,
            title: "Top",
            body: "",
            head_branch: "top",
            head_sha: &top,
            base_branch: "upper",
            base_sha: &head,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    db::pull_requests::records::join_pull_request_stack(w.database.pool(), w.alice_id, new.id)
        .await
        .unwrap();
    let ci = ci(&w);
    pulls::queue_pull_request_ci(
        w.database.pool(),
        &ci,
        &w.repository,
        &row(&w, new.number).await,
        w.alice_id,
        false,
        db::models::AccessLevel::Owner,
    )
    .await
    .unwrap();
    assert!(db::ci::latest_ci_runs_for_commit(
        w.database.pool(),
        w.repository.id,
        &top,
        new.id,
        "upper"
    )
    .await
    .unwrap()
    .is_empty());
    w.alice
        .expect(
            "DELETE",
            "/api/repos/alice/proj/pulls/3/stack",
            json!({}),
            StatusCode::OK,
        )
        .await;
    let runs = db::ci::latest_ci_runs_for_commit(
        w.database.pool(),
        w.repository.id,
        &top,
        new.id,
        "upper",
    )
    .await
    .unwrap();
    assert!(
        runs.iter()
            .any(|run| run.workflow_path == ".github/workflows/own.yml"),
        "unstack must queue members added after the earlier snapshot"
    );
    w.database.finish().await;
}

async fn merge_followup(pg: bool) {
    let mut w = World::new(pg).await;
    let head = save_text(&w, "topic", ".github/workflows/ci.yml", WORKFLOW, &w.head).await;
    w.database
        .execute(
            "UPDATE pull_requests SET is_draft=FALSE WHERE id=$1",
            &[w.pull.id],
        )
        .await;
    // Neither the head nor the base of this PR is changed by the merge.
    w.alice.branch("release", &w.base).await;
    w.alice.branch("unrelated", &head).await;
    let other = w.alice.open_draft("Other", "unrelated", "release").await;
    {
        let git = git2::Repository::open_bare(&w.repository.disk_path).unwrap();
        git.reference(
            "refs/heads/unrelated",
            git2::Oid::from_str(&w.base).unwrap(),
            true,
            "move",
        )
        .unwrap();
    }
    metadata_failure(&w, pg).await;
    let current = w.alice.pull(1).await;
    let value = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/merge",
            json!({"expected_head":head,"expected_base":current["base_sha"], "method":"merge"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(value["merged"], true);
    assert!(value["message"]
        .as_str()
        .unwrap()
        .contains("bookkeeping failed"));
    assert_eq!(row(&w, 1).await.state, "merged");
    assert_eq!(
        row(&w, other["number"].as_i64().unwrap() as i32)
            .await
            .head_sha,
        head,
        "unrelated PR must not be synchronously refreshed"
    );
    let merged = repo_store(&w.repository, None)
        .commit("main")
        .await
        .unwrap()
        .sha;
    let runs = db::ci::list_ci_runs_for_repo(w.database.pool(), w.repository.id, 100)
        .await
        .unwrap();
    assert!(runs
        .iter()
        .any(|run| run.event == "push" && run.head_sha.as_deref() == Some(&merged)));
    w.database.finish().await;
}

async fn restack_followup(pg: bool) {
    let mut w = World::new(pg).await;
    let head = save_text(&w, "topic", ".github/workflows/ci.yml", WORKFLOW, &w.head).await;
    w.alice.branch("upper", &head).await;
    let upper_head = w.alice.save("upper", "upper.txt", &head).await;
    w.alice.open_draft("Upper", "upper", "topic").await;
    w.alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/2/stack",
            json!({}),
            StatusCode::OK,
        )
        .await;
    w.alice.save("main", "new-main.txt", &w.base).await;
    w.database
        .execute("UPDATE pull_requests SET is_draft=FALSE", &[])
        .await;
    metadata_failure(&w, pg).await;
    let value = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/2/stack/rebase",
            json!({}),
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    assert_eq!(value["error"], "restack_followup_failed");
    let runs = db::ci::list_ci_runs_for_repo(w.database.pool(), w.repository.id, 100)
        .await
        .unwrap();
    for number in [1, 2] {
        let pull = row(&w, number).await;
        assert_ne!(
            pull.head_sha,
            if number == 1 {
                head.as_str()
            } else {
                upper_head.as_str()
            }
        );
        assert_eq!(
            repo_store(&w.repository, None)
                .commit(&format!("refs/heads/{}", pull.head_branch))
                .await
                .unwrap()
                .sha,
            pull.head_sha
        );
        assert!(runs.iter().any(|run| run.event == "pull_request"
            && run.pull_request_id == Some(pull.id)
            && run.head_sha.as_deref() == Some(&pull.head_sha)));
        assert!(runs
            .iter()
            .any(|run| run.event == "push" && run.head_sha.as_deref() == Some(&pull.head_sha)));
    }
    w.database.finish().await;
}

async fn target_specific_checks(pg: bool) {
    let mut w = World::new(pg).await;
    let workflow = "on:\n  pull_request:\n    branches: [main]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n";
    let head = save_text(&w, "topic", ".github/workflows/ci.yml", workflow, &w.head).await;
    w.database
        .execute(
            "UPDATE pull_requests SET is_draft=FALSE WHERE id=$1",
            &[w.pull.id],
        )
        .await;
    let ci = ci(&w);
    pulls::queue_pull_request_ci(
        w.database.pool(),
        &ci,
        &w.repository,
        &row(&w, 1).await,
        w.alice_id,
        false,
        db::models::AccessLevel::Owner,
    )
    .await
    .unwrap();
    w.database
        .execute("UPDATE ci_runs SET status='success'", &[])
        .await;
    w.database
        .execute("UPDATE commit_statuses SET state='success'", &[])
        .await;
    w.alice.branch("release", &w.base).await;
    let current = w.alice.pull(1).await;
    w.alice.expect("PUT", "/api/repos/alice/proj/pulls/1", json!({"expected_state":"open", "expected_updated":current["updated_at"], "base":"release"}), StatusCode::OK).await;
    assert!(db::ci::latest_ci_runs_for_commit(
        w.database.pool(),
        w.repository.id,
        &head,
        w.pull.id,
        "release"
    )
    .await
    .unwrap()
    .is_empty());
    assert!(
        db::ci::list_pull_request_check_statuses(w.database.pool(), &[w.pull.id])
            .await
            .unwrap()
            .is_empty()
    );
    let model = bounded_model(&w.alice, "/alice/proj/pulls/1/checks").await;
    let Page::Collaboration(model) = model.page else {
        panic!("not collaboration")
    };
    let issues::Page::PullChecks { runs, statuses, .. } = *model else {
        panic!("not checks")
    };
    assert!(runs.is_empty());
    assert!(
        statuses.is_empty(),
        "a previous target's run must not reappear as a standalone status"
    );
    w.alice
        .expect(
            "PUT",
            "/api/repos/alice/proj/branch-protection",
            json!({"pattern":"release", "require_status_checks":true,
        "required_status_checks":["gitserv/workflow/.github/workflows/ci.yml"]}),
            StatusCode::OK,
        )
        .await;
    let current = w.alice.pull(1).await;
    let denied = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls/1/merge",
            json!({"expected_head":head, "expected_base":current["base_sha"], "method":"merge"}),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(denied["error"], "branch_protection");
    assert!(denied["message"].as_str().unwrap().contains("missing"));
    w.database.finish().await;
}

async fn refs_prefixed_base(pg: bool) {
    let mut w = World::new(pg).await;
    let workflow = "on:\n  pull_request:\n    branches: ['refs/**']\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n";
    let base = save_text(&w, "main", ".github/workflows/ci.yml", workflow, &w.base).await;
    for (index, branch) in ["refs/topic", "refs/heads/nested"].into_iter().enumerate() {
        w.alice.branch(branch, &base).await;
        let head_branch = format!("ci-head-{index}");
        w.alice.branch(&head_branch, &base).await;
        let head = w.alice.save(&head_branch, "change.txt", &base).await;
        let pull = w.alice.open_pull("CI", &head_branch, branch).await;
        let id = pull["id"].as_i64().unwrap();
        let runs = db::ci::latest_ci_runs_for_commit(
            w.database.pool(),
            w.repository.id,
            &head,
            id,
            branch,
        )
        .await
        .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].branch, branch);
        ci(&w)
            .dispatch_run(
                w.alice_id,
                codeyard::backend::ci::runs::DispatchRequest {
                    repo_owner: "alice".into(),
                    repo_name: "proj".into(),
                    branch: branch.into(),
                    event: "pull_request".into(),
                    head_sha: Some(head.clone()),
                    workflow_path: Some(".github/workflows/ci.yml".into()),
                    pipeline: None,
                    environment: None,
                    inputs: None,
                },
            )
            .await
            .unwrap();
        w.alice
            .expect(
                "POST",
                &format!("/api/repos/alice/proj/pulls/{}/workflows", pull["number"]),
                json!({"expected_head":head}),
                StatusCode::OK,
            )
            .await;
    }
    w.database.finish().await;
}

async fn path_failure_keeps_unfiltered(pg: bool) {
    let mut w = World::new(pg).await;
    let head = save_text(
        &w,
        "topic",
        ".github/workflows/plain.yml",
        WORKFLOW,
        &w.head,
    )
    .await;
    let filtered = "on:\n  pull_request:\n    paths: ['bulk/**']\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n";
    let head = save_text(
        &w,
        "topic",
        ".github/workflows/filtered.yml",
        filtered,
        &head,
    )
    .await;
    let big = {
        let git = git2::Repository::open_bare(&w.repository.disk_path).unwrap();
        let parent = git
            .find_commit(git2::Oid::from_str(&head).unwrap())
            .unwrap();
        let mut bulk = git.treebuilder(None).unwrap();
        let blob = git.blob(b"x\n").unwrap();
        for n in 0..8193 {
            bulk.insert(format!("file-{n}"), blob, 0o100644).unwrap();
        }
        let mut tree = git.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        tree.insert("bulk", bulk.write().unwrap(), 0o040000)
            .unwrap();
        let tree = git.find_tree(tree.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Alice", "alice@example.invalid").unwrap();
        git.commit(
            Some("refs/heads/topic"),
            &signature,
            &signature,
            "Many paths",
            &tree,
            &[&parent],
        )
        .unwrap()
        .to_string()
    };
    w.database
        .execute(
            "UPDATE pull_requests SET is_draft=FALSE WHERE id=$1",
            &[w.pull.id],
        )
        .await;
    let ci = ci(&w);
    pulls::refresh_open_pull_request(
        w.database.pool(),
        &w.repository,
        &ci,
        w.alice_id,
        w.pull.id,
        Some(db::models::AccessLevel::Owner),
    )
    .await
    .unwrap();
    let runs = db::ci::latest_ci_runs_for_commit(
        w.database.pool(),
        w.repository.id,
        &big,
        w.pull.id,
        "main",
    )
    .await
    .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].workflow_path, ".github/workflows/plain.yml");
    assert!(
        pulls::queue_pull_request_ci(
            w.database.pool(),
            &ci,
            &w.repository,
            &row(&w, 1).await,
            w.alice_id,
            true,
            db::models::AccessLevel::Owner,
        )
        .await
        .is_err(),
        "the path-filtered workflow failure is still reported"
    );
    w.database.finish().await;
}

async fn nested_comment_links(pg: bool) {
    for (count, bytes) in [(101, 16), (4, 60000)] {
        let mut w = World::new(pg).await;
        w.database
            .execute(
                "INSERT INTO pr_reviews(pull_request_id, author_id, state) VALUES($1,$2,'pending')",
                &[w.pull.id, w.alice_id],
            )
            .await;
        let review = db::pull_requests::reviews::get_pending_review(
            w.database.pool(),
            w.pull.id,
            w.alice_id,
        )
        .await
        .unwrap()
        .unwrap();
        let body = if pg {
            format!("repeat('x', {bytes})")
        } else {
            format!("replace(hex(zeroblob({})), '0', 'x')", bytes / 2)
        };
        let sql = format!("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<{count}) INSERT INTO pr_comments(pull_request_id,author_id,review_id,body,path,line,side,commit_sha) SELECT $1,$2,$3, CASE WHEN x={count} THEN '@bob last comment {count}' ELSE {body} END, 'topic.txt',1,'RIGHT','{}' FROM n", w.head);
        w.database
            .execute(&sql, &[w.pull.id, w.alice_id, review.id])
            .await;
        w.alice
            .expect(
                "POST",
                "/api/repos/alice/proj/pulls/1/reviews",
                json!({"state":"commented", "body":"Summary", "commit_sha":w.head}),
                StatusCode::CREATED,
            )
            .await;
        let notices = db::notifications::records::list_notifications_for_user(
            w.database.pool(),
            w.bob_id,
            100,
            false,
        )
        .await
        .unwrap();
        let notice = notices
            .iter()
            .find(|notice| {
                notice
                    .body
                    .as_deref()
                    .is_some_and(|body| body.contains(&format!("last comment {count}")))
            })
            .unwrap();
        let link = notice.link.as_ref().unwrap();
        // The stored link names the comment; following it lands on the comment's page and its
        // review comment offset.
        assert!(link.contains("?comment="), "{link}");
        let id = notice.metadata["pull_request_comment_id"].as_i64().unwrap();
        let followed = w
            .alice
            .exchange(
                "GET",
                link.split('#').next().unwrap(),
                "text/html",
                Vec::new(),
            )
            .await;
        let location = followed
            .location
            .expect("the link redirects to the comment");
        assert!(location.contains("review_comments="), "{location}");
        let model = bounded_model(&w.alice, &location).await;
        let Page::Collaboration(model) = model.page else {
            panic!("not collaboration")
        };
        let issues::Page::Detail { comments, .. } = *model else {
            panic!("not detail")
        };
        assert!(
            comments
                .reviews
                .iter()
                .flat_map(|review| &review.comments)
                .any(|comment| comment.id == id),
            "notification target must be present"
        );
        w.database.finish().await;
    }
}

async fn head_capacity_reports_opened(pg: bool) {
    let mut w = World::new(pg).await;
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted\n");
    // Packed refs keep this capacity boundary test cheap.
    for n in 100000..140000 {
        packed.push_str(&format!("{} refs/pull/{n}/head\n", w.head));
    }
    std::fs::write(
        std::path::Path::new(&w.repository.disk_path).join("packed-refs"),
        packed,
    )
    .unwrap();
    w.alice.branch("overflow", &w.head).await;
    let value = w
        .alice
        .expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title":"Overflow", "head_branch":"overflow", "base_branch":"main"}),
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    assert_eq!(value["error"], "git_unavailable");
    assert!(value["message"].as_str().unwrap().contains("opened"));
    assert_eq!(row(&w, 2).await.state, "open");
    w.database.finish().await;
}

macro_rules! contracts {
    ($($sqlite:ident, $postgres:ident => $contract:ident;)*) => {$ (
        #[tokio::test] async fn $sqlite() { $contract(false).await; }
        #[tokio::test] async fn $postgres() { $contract(true).await; }
    )*};
}
contracts! {
    sqlite_removed_author_refresh, postgres_removed_author_refresh => removed_author;
    sqlite_inline_revision_conflicts, postgres_inline_revision_conflicts => inline_revision;
    sqlite_deleted_comment_notifications, postgres_deleted_comment_notifications => deleted_comment_notifications;
    sqlite_creation_recovery_admission, postgres_creation_recovery_admission => recovery;
    sqlite_stack_owner_and_path_target, postgres_stack_owner_and_path_target => stack_target;
    sqlite_merge_followup_and_branch_filter, postgres_merge_followup_and_branch_filter => merge_followup;
    sqlite_restack_followup, postgres_restack_followup => restack_followup;
    sqlite_target_specific_checks, postgres_target_specific_checks => target_specific_checks;
    sqlite_refs_prefixed_base, postgres_refs_prefixed_base => refs_prefixed_base;
    sqlite_path_failure_keeps_unfiltered, postgres_path_failure_keeps_unfiltered => path_failure_keeps_unfiltered;
    sqlite_nested_comment_notification_links, postgres_nested_comment_notification_links => nested_comment_links;
    sqlite_head_capacity_partial_creation, postgres_head_capacity_partial_creation => head_capacity_reports_opened;
}
