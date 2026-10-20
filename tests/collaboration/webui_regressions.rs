use super::templates::setup;
use axum::http::StatusCode;
use codeyard::{
    backend::db::{self, assignees::AssignedItem, models::AccessLevel},
    webui::{app::Page, pages::repos::issues},
};
use serde_json::{json, Value};

async fn comment_edit_snapshots(pg: bool) {
    let (mut database, _root, alice, _bob) = setup(pg).await;
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title":"Edits"}),
            StatusCode::CREATED,
        )
        .await;
    let tip = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &tip).await;
    alice.save("topic", "change.txt", &tip).await;
    let pull = alice.open_pull("Edits", "topic", "main").await;
    for (family, number) in [
        ("issues", issue["number"].as_i64().unwrap()),
        ("pulls", pull["number"].as_i64().unwrap()),
    ] {
        let api = format!("/api/repos/alice/proj/{family}/{number}/comments");
        let original = "- [ ] A\n- [ ] B";
        let comment = alice
            .expect("POST", &api, json!({"body":original}), StatusCode::CREATED)
            .await;
        let id = comment["id"].as_i64().unwrap();
        let edit = format!("/alice/proj/{family}/{number}/comments/{id}/edit");
        let expected = serde_json::to_string(original).unwrap();
        let saved = alice
            .form_reply(
                &edit,
                &[("body", "First tab"), ("expected_body", &expected)],
            )
            .await;
        assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
        let stale = alice
            .form_reply(
                &edit,
                &[("body", "Stale second tab"), ("expected_body", &expected)],
            )
            .await;
        assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.text());
        assert!(stale.text().contains("Stale second tab"));
        let comments = alice.expect("GET", &api, Value::Null, StatusCode::OK).await;
        assert_eq!(comments[0]["body"], "First tab");

        database
            .execute(
                "UPDATE repositories SET is_archived=TRUE WHERE name='proj'",
                &[],
            )
            .await;
        let refused = alice
            .form_reply(
                &edit,
                &[
                    ("body", "Keep archived edit"),
                    ("expected_body", "\"First tab\""),
                ],
            )
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
        let html = refused.text();
        assert!(html.contains("Comment text that was not saved"));
        assert!(html.contains("readonly"));
        assert!(html.contains("Keep archived edit"));
        database
            .execute(
                "UPDATE repositories SET is_archived=FALSE WHERE name='proj'",
                &[],
            )
            .await;
        let deleted = alice
            .form_reply(
                &format!("/alice/proj/{family}/{number}/comments/{id}/delete"),
                &[],
            )
            .await;
        assert_eq!(deleted.status, StatusCode::SEE_OTHER);
        let missing = alice
            .form_reply(
                &edit,
                &[
                    ("body", "Keep deleted edit"),
                    ("expected_body", "\"First tab\""),
                ],
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.text());
        assert!(missing.text().contains("Keep deleted edit"));
        assert!(missing.text().contains("Comment text that was not saved"));
    }
    database.finish().await;
}

async fn independent_people_filters(pg: bool) {
    let (mut database, _root, alice, _bob) = setup(pg).await;
    let pool = database.pool();
    let repo = db::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let owner = repo.owner_user_id.unwrap();
    let none = database.actor("none").await;
    let writer = database.actor("writer").await;
    let bob = db::users::accounts::get_user_by_username(pool, "bob")
        .await
        .unwrap()
        .unwrap();
    for (id, level) in [
        (none, AccessLevel::Read),
        (bob.id, AccessLevel::Read),
        (writer, AccessLevel::Write),
    ] {
        db::repositories::access::grant_repo_access(pool, id, repo.id, level)
            .await
            .unwrap();
    }
    let assigned = db::issues::create_issue(pool, repo.id, owner, "Assigned to none", "")
        .await
        .unwrap();
    let unassigned = db::issues::create_issue(pool, repo.id, owner, "Unassigned", "")
        .await
        .unwrap();
    db::assignees::assign_user(pool, owner, repo.id, AssignedItem::Issue(assigned.id), none)
        .await
        .unwrap();
    for (query, number) in [
        ("assignee=%40none", assigned.number),
        ("q=assignee%3Anone", assigned.number),
        ("q=no%3Aassignee", unassigned.number),
        ("assignee=none", unassigned.number),
    ] {
        let (status, page) = alice.model(&format!("/alice/proj/issues?{query}")).await;
        assert_eq!(status, StatusCode::OK);
        let Page::Collaboration(page) = page else {
            panic!("collaboration page")
        };
        let issues::Page::List { items, .. } = *page else {
            panic!("issue list")
        };
        assert_eq!(
            items.iter().map(|item| item.number).collect::<Vec<_>>(),
            [number]
        );
    }
    let (_, html) = alice.page("/alice/proj/issues").await;
    assert!(html.contains("assignee=%40none"));

    let tip = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &tip).await;
    alice.save("topic", "change.txt", &tip).await;
    let pull = alice.open_pull("Review", "topic", "main").await;
    db::assignees::assign_user(
        pool,
        owner,
        repo.id,
        AssignedItem::PullRequest(pull["id"].as_i64().unwrap()),
        writer,
    )
    .await
    .unwrap();
    let (status, page) = alice
        .model(&format!(
            "/alice/proj/pulls/{}",
            pull["number"].as_i64().unwrap()
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page:?}");
    let Page::Collaboration(page) = page else {
        panic!("collaboration page")
    };
    let issues::Page::Detail {
        reviewer_choices,
        assignable,
        ..
    } = *page
    else {
        panic!("detail")
    };
    assert!(reviewer_choices.iter().any(|name| name == "writer"));
    assert!(!reviewer_choices
        .iter()
        .any(|name| name == "bob" || name == "none"));
    assert!(!assignable.iter().any(|name| name == "writer"));
    database.finish().await;
}

#[tokio::test]
async fn native_comment_edits_keep_their_original_text_and_recover_refused_drafts() {
    comment_edit_snapshots(false).await;
    comment_edit_snapshots(true).await;
}

#[tokio::test]
async fn reviewer_choices_and_the_account_named_none_are_independent_filters() {
    independent_people_filters(false).await;
    independent_people_filters(true).await;
}
