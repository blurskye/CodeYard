use super::templates::{hidden_value, put, setup};
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::db;
use serde_json::{json, Value};

async fn stale_issue_forms(pg: bool) {
    let (mut database, root, alice, bob) = setup(pg).await;
    let pool = database.pool();
    let repo = db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let reader = db::users::accounts::get_user_by_username(pool, "bob")
        .await
        .unwrap()
        .unwrap();
    db::repositories::access::grant_repo_access(
        pool,
        reader.id,
        repo.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    database
        .execute(
            "UPDATE repositories SET is_private=TRUE WHERE id=$1",
            &[repo.id],
        )
        .await;
    let (_, old) = bob.page("/alice/proj/issues/new").await;
    let original_id = hidden_value(&old, "repository_id");
    database
        .execute(
            "UPDATE repositories SET name='moved' WHERE id=$1",
            &[repo.id],
        )
        .await;
    let replacement = codeyard::backend::repositories::lifecycle::provision(
        (pool, None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(repo.owner_user_id.unwrap()),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let refused = bob
        .form_reply(
            "/alice/proj/issues/new",
            &[
                ("repository_id", &original_id),
                ("title", "Private draft"),
                ("body", "Confidential notes"),
                ("intent", "create"),
            ],
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text()
    );
    assert!(refused.text().contains("Confidential notes"));
    assert!(
        db::issues::list_repo_issues(pool, replacement.id, &Default::default(), 100)
            .await
            .unwrap()
            .is_empty()
    );
    let (status, _) = bob
        .api(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title":"Private draft", "repository_id": repo.id}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let template = ".github/ISSUE_TEMPLATE/form.yml";
    let original = "name: Report\nbody:\n  - type: input\n    attributes:\n      label: Version\n  - type: textarea\n    attributes:\n      label: Steps\n";
    let tip = put(&alice, "main", template, original, "unborn").await;
    let (_, page) = bob
        .page("/alice/proj/issues/new?template=.github%2FISSUE_TEMPLATE%2Fform.yml")
        .await;
    let revision = hidden_value(&page, "template_revision");
    let mut tip = tip;
    for changed in [
        original
            .replace("label: Version", "label: Steps")
            .replace("label: Steps\n", "label: Changed\n"),
        "not an issue form".into(),
    ] {
        let updated = alice.expect(
            "PUT", &format!("/api/repos/alice/proj/contents/{template}"),
            json!({"branch":"main", "content":base64::engine::general_purpose::STANDARD.encode(&changed), "message":"Change issue form", "expected_revision":tip}),
            StatusCode::OK,
        ).await;
        tip = updated["commit"]["sha"].as_str().unwrap().to_owned();
        let reply = bob
            .form_reply(
                "/alice/proj/issues/new",
                &[
                    ("template", template),
                    ("template_revision", &revision),
                    ("title", "Report"),
                    ("form_0", "private version"),
                    ("form_1", "carefully written steps"),
                    ("intent", "create"),
                ],
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text()
        );
        assert!(reply.text().contains("template changed or was removed"));
        assert!(reply.text().contains("private version"));
        assert!(reply.text().contains("carefully written steps"));
        assert!(
            db::issues::list_repo_issues(pool, replacement.id, &Default::default(), 100)
                .await
                .unwrap()
                .is_empty()
        );
    }
    database.finish().await;
}

async fn comment_tasks_check_scope_and_original_body(pg: bool) {
    let (mut database, _root, alice, bob) = setup(pg).await;
    let issue = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title":"Reader's tasks", "body":"- [ ] author task"}),
            StatusCode::CREATED,
        )
        .await;
    let (_, page) = bob.page("/alice/proj/issues/1").await;
    assert!(!page.contains("form=\"task-body\""));
    let tip = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &tip).await;
    alice.save("topic", "change.txt", &tip).await;
    let pull = alice.open_pull("Tasks", "topic", "main").await;
    let original = "- [ ] A\n- [ ] B";
    for (family, number) in [
        ("issues", issue["number"].as_i64().unwrap()),
        ("pulls", pull["number"].as_i64().unwrap()),
    ] {
        let api = format!("/api/repos/alice/proj/{family}/{number}/comments");
        let comment = alice
            .expect("POST", &api, json!({"body":original}), StatusCode::CREATED)
            .await;
        let id = comment["id"].as_i64().unwrap().to_string();
        let tasks = format!("/alice/proj/{family}/{number}/tasks");
        for index in ["0", "999"] {
            let refused = bob
                .form_reply(
                    &tasks,
                    &[
                        ("comment", &id),
                        ("task", index),
                        ("comment_revision", original),
                    ],
                )
                .await;
            assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.text());
        }
        let first_form = [
            ("comment", id.as_str()),
            ("task", "0"),
            ("comment_revision", original),
        ];
        let second_form = [
            ("comment", id.as_str()),
            ("task", "1"),
            ("comment_revision", original),
        ];
        let (first, second) = tokio::join!(
            alice.form_reply(&tasks, &first_form),
            alice.form_reply(&tasks, &second_form)
        );
        assert_eq!(
            [first.status, second.status]
                .iter()
                .filter(|status| **status == StatusCode::SEE_OTHER)
                .count(),
            1
        );
        assert_eq!(
            [first.status, second.status]
                .iter()
                .filter(|status| **status == StatusCode::CONFLICT)
                .count(),
            1
        );
        let reordered = "- [ ] X\n- [ ] A\n- [ ] B";
        let edited = alice
            .form_reply(
                &format!("/alice/proj/{family}/{number}/comments/{id}/edit"),
                &[("body", reordered)],
            )
            .await;
        assert_eq!(edited.status, StatusCode::SEE_OTHER, "{}", edited.text());
        let stale = alice
            .form_reply(
                &tasks,
                &[
                    ("comment", &id),
                    ("task", "1"),
                    ("comment_revision", original),
                ],
            )
            .await;
        assert_eq!(stale.status, StatusCode::CONFLICT);
        let saved = alice.expect("GET", &api, Value::Null, StatusCode::OK).await;
        assert_eq!(
            saved
                .as_array()
                .unwrap()
                .iter()
                .find(|comment| comment["id"].as_i64().unwrap().to_string() == id)
                .unwrap()["body"],
            reordered
        );
    }
    // A comment on another issue is never parsed, even when both issues are readable.
    let other = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title":"Other"}),
            StatusCode::CREATED,
        )
        .await;
    let comment = alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/issues/{}/comments", other["number"]),
            json!({"body":original}),
            StatusCode::CREATED,
        )
        .await;
    for index in ["0", "999"] {
        let reply = alice
            .form_reply(
                "/alice/proj/issues/1/tasks",
                &[
                    ("comment", &comment["id"].to_string()),
                    ("task", index),
                    ("comment_revision", original),
                ],
            )
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }
    database.finish().await;
}

async fn later_page_edit_and_deleted_notifications(pg: bool) {
    let (mut database, _root, alice, bob) = setup(pg).await;
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title":"Discussion"}),
            StatusCode::CREATED,
        )
        .await;
    let author = db::users::accounts::get_user_by_username(database.pool(), "bob")
        .await
        .unwrap()
        .unwrap();
    let mut last = None;
    for _ in 0..31 {
        last = Some(
            db::issues::add_issue_comment(
                database.pool(),
                issue["id"].as_i64().unwrap(),
                author.id,
                "Existing comment",
            )
            .await
            .unwrap(),
        );
    }
    let comment = last.unwrap();
    let (_, page) = bob.page("/alice/proj/issues/1?page=2").await;
    assert!(page.contains(&format!("comments/{}/edit?page=2", comment.id)));
    for pairs in [
        vec![("body", "Edit draft on page two"), ("intent", "preview")],
        vec![("body", " ")],
    ] {
        let reply = bob
            .form_reply(
                &format!("/alice/proj/issues/1/comments/{}/edit?page=2", comment.id),
                &pairs,
            )
            .await;
        assert!(reply
            .text()
            .contains(&format!("edit-comment-{}", comment.id)));
        let text = reply.text();
        let composer = text
            .split(
                "class=\"collaboration-panel collaboration-comment-form collaboration-composer\"",
            )
            .nth(1)
            .unwrap()
            .split("</form>")
            .next()
            .unwrap();
        assert!(!composer.contains("Edit draft on page two"));
    }
    let removed = bob
        .form_reply(
            &format!("/alice/proj/issues/1/comments/{}/delete", comment.id),
            &[],
        )
        .await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER);
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    for kind in ["issue_comment", "mention"] {
        let delayed = db::notifications::records::create_notification(
            database.pool(),
            db::notifications::records::NewNotification {
                user_id: repo.owner_user_id.unwrap(),
                actor_id: Some(author.id),
                repo_id: Some(repo.id),
                kind,
                title: "Late notice",
                body: Some("Deleted text"),
                link: Some("/alice/proj/issues/1#comment"),
                metadata: &json!({"comment_id":comment.id}),
            },
        )
        .await;
        assert!(
            delayed.is_err(),
            "deleted comment generated a {kind} notification"
        );
    }
    database.finish().await;
}

#[tokio::test]
async fn sqlite_stale_issue_forms() {
    stale_issue_forms(false).await;
}
#[tokio::test]
async fn postgres_stale_issue_forms() {
    stale_issue_forms(true).await;
}
#[tokio::test]
async fn sqlite_comment_tasks_check_scope_and_original_body() {
    comment_tasks_check_scope_and_original_body(false).await;
}
#[tokio::test]
async fn postgres_comment_tasks_check_scope_and_original_body() {
    comment_tasks_check_scope_and_original_body(true).await;
}
#[tokio::test]
async fn sqlite_later_page_edit_and_deleted_notifications() {
    later_page_edit_and_deleted_notifications(false).await;
}
#[tokio::test]
async fn postgres_later_page_edit_and_deleted_notifications() {
    later_page_edit_and_deleted_notifications(true).await;
}
