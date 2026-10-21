//! Issue, pull request, comment and review text, repository descriptions, oversized
//! comparisons and the collaborator list, through the JSON API and the native forms.
//! Blank or oversized text is refused on both paths and nothing is stored. On SQLite
//! and PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use base64::Engine;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use std::sync::Arc;

async fn contract(pg: bool) {
    // The native forms answer with rendered pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
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
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let long = "x".repeat(65_537);

    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Works", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    let number = issue["number"].as_i64().unwrap();
    for (title, body) in [
        ("   ", ""),
        ("Title", long.as_str()),
        ("Ti\0tle", ""),
        ("Title", "Bo\0dy"),
    ] {
        let (status, value) = alice
            .api(
                "POST",
                "/api/repos/alice/proj/issues",
                json!({"title": title, "body": body}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"], "validation_error", "{value}");
        let (status, page) = alice
            .form(
                "/alice/proj/issues/new",
                &[("title", title), ("body", body)],
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    }
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");

    let comments = format!("/api/repos/alice/proj/issues/{number}/comments");
    for body in ["   ", long.as_str(), "Bo\0dy"] {
        let (status, value) = alice.api("POST", &comments, json!({"body": body})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"], "validation_error", "{value}");
        let (status, page) = alice
            .form(
                &format!("/alice/proj/issues/{number}/comments"),
                &[("body", body)],
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    }
    let listed = alice
        .expect("GET", &comments, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 0, "{listed}");

    let main = alice.save("main", "README.md", "unborn").await;
    alice.branch("topic", &main).await;
    alice.save("topic", "topic.txt", &main).await;
    let pull = alice.open_pull("Topic", "topic", "main").await;
    let pull_number = pull["number"].as_i64().unwrap();
    // The JSON pull list takes no label parameter, so it ignores one. The page filters by label.
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/pulls?labels=bug",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let (status, page) = alice.page("/alice/proj/pulls?label=bug").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("label:bug"), "{page}");
    assert!(!page.contains("Topic"), "{page}");
    for (title, body) in [
        ("   ", ""),
        ("Title", long.as_str()),
        ("Ti\0tle", ""),
        ("Title", "Bo\0dy"),
    ] {
        let (status, value) = alice
            .api(
                "POST",
                "/api/repos/alice/proj/pulls",
                json!({"title": title, "body": body, "head_branch": "topic", "base_branch": "main"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"], "validation_error", "{value}");
    }
    let pull_comments = format!("/api/repos/alice/proj/pulls/{pull_number}/comments");
    for body in ["   ", long.as_str(), "Bo\0dy"] {
        let (status, value) = alice
            .api("POST", &pull_comments, json!({"body": body}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"], "validation_error", "{value}");
        let (status, page) = alice
            .form(
                &format!("/alice/proj/pulls/{pull_number}/comments"),
                &[("body", body)],
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    }
    let listed = alice
        .expect("GET", &pull_comments, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 0, "{listed}");

    // Review bodies and dismissal reasons take the comment limit. A comment review needs a
    // body; an approval may leave it out.
    let reviews = format!("/api/repos/alice/proj/pulls/{pull_number}/reviews");
    let (status, value) = alice
        .api(
            "POST",
            &reviews,
            json!({"state": "commented", "body": long.as_str()}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");
    let listed = alice
        .expect("GET", &reviews, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 0, "{listed}");
    let review = alice
        .expect(
            "POST",
            &reviews,
            json!({"state": "commented", "body": "x".repeat(65_536)}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(review["state"], "commented");
    let repo = db::repositories::records::get_repository(f.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let reviewer = Client::login(&router, f.pool(), bob_id).await;
    let vote = reviewer
        .expect(
            "POST",
            &reviews,
            json!({"state":"approved", "body":"Approval"}),
            StatusCode::CREATED,
        )
        .await;
    let dismiss = format!("{reviews}/{}/dismiss", vote["id"]);
    let (status, value) = alice
        .api("POST", &dismiss, json!({"reason": long.as_str()}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");
    let dismissed = alice
        .expect(
            "POST",
            &dismiss,
            json!({"reason": "x".repeat(65_536)}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(dismissed["state"], "dismissed", "{dismissed}");
    // A comment review needs a body; an empty one is refused and nothing is stored.
    let (status, value) = alice
        .api("POST", &reviews, json!({"state": "commented", "body": ""}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");
    alice
        .expect(
            "POST",
            &reviews,
            json!({"state": "commented", "body": "Reviewed"}),
            StatusCode::CREATED,
        )
        .await;

    // A comparison over the patch budget is too large to show, not unavailable.
    alice.branch("big", &main).await;
    let big = base64::engine::general_purpose::STANDARD.encode("changed line\n".repeat(25_000));
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/contents/big.txt",
            json!({"content": big, "message": "Add big.txt", "branch": "big", "expected_revision": main.as_str()}),
            StatusCode::CREATED,
        )
        .await;
    let (status, page) = alice
        .form(
            "/alice/proj/pulls/new",
            &[("head", "big"), ("base", "main"), ("intent", "compare")],
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{page}");

    // NUL is refused in review text and dismissal reasons, as in comments.
    let (status, value) = alice
        .api(
            "POST",
            &reviews,
            json!({"state": "commented", "body": "Bo\0dy"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");
    let (status, review) = alice
        .api(
            "POST",
            &reviews,
            json!({"state": "commented", "body": "Looks fine"}),
        )
        .await;
    assert!(status.is_success(), "{review}");
    let (status, value) = alice
        .api(
            "POST",
            &format!("{reviews}/{}/dismiss", review["id"]),
            json!({"reason": "Bo\0dy"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");

    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/collaborators",
            json!({"username": "bob", "access_level": "write"}),
            StatusCode::OK,
        )
        .await;
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/collaborators",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let bob = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["username"] == "bob")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(bob["user_id"], bob_id);
    assert_eq!(bob["access_level"], "write");

    // Comment counts and labels read the same on the issue, the issue list and an update.
    let (status, value) = alice
        .api("POST", &comments, json!({"body": "First comment"}))
        .await;
    assert!(status.is_success(), "{value}");
    let commented_issue = value["issue_id"].clone();
    let comment_list = alice
        .expect("GET", &comments, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(comment_list.as_array().unwrap().len(), 1, "{comment_list}");
    for (name, description) in [("Na\0me", ""), ("bug", "De\0scription")] {
        let (status, value) = alice
            .api(
                "POST",
                "/api/repos/alice/proj/labels",
                json!({"name": name, "color": "d73a4a", "description": description}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"], "validation_error", "{value}");
        let (status, page) = alice
            .form(
                "/alice/proj/_forms/label",
                &[
                    (
                        "repository_id",
                        &db::repositories::records::get_repository(f.pool(), "alice", "proj")
                            .await
                            .unwrap()
                            .unwrap()
                            .id
                            .to_string(),
                    ),
                    ("name", name),
                    ("color", "d73a4a"),
                    ("description", description),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    }
    let label = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/labels",
            json!({"name": "bug", "color": "d73a4a"}),
            StatusCode::CREATED,
        )
        .await;
    let (status, value) = alice
        .api(
            "POST",
            &format!("/api/repos/alice/proj/issues/{number}/labels"),
            json!({"labels": [label["id"]]}),
        )
        .await;
    assert!(status.is_success(), "{value}");
    let read = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/issues/{number}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(read["id"], commented_issue, "{read}");
    assert_eq!(read["comments_count"], 1, "{read}");
    assert_eq!(read["labels"][0]["name"], "bug", "{read}");
    let updated = alice
        .expect(
            "PUT",
            &format!("/api/repos/alice/proj/issues/{number}"),
            json!({"expected_state": "open", "expected_updated": read["updated_at"], "title": "Works again"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(updated["comments_count"], 1, "{updated}");
    assert_eq!(updated["labels"][0]["name"], "bug", "{updated}");
    let listed = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/issues",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed[0]["comments_count"], 1, "{listed}");
    assert_eq!(listed[0]["labels"][0]["name"], "bug", "{listed}");

    // A tag's JSON listing keeps its annotation.
    let (status, value) = alice
        .api(
            "POST",
            "/api/repos/alice/proj/tags",
            json!({"name": "v1", "sha": main, "message": "Release notes"}),
        )
        .await;
    assert!(status.is_success(), "{value}");
    let tags = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/tags",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    let tag = tags
        .as_array()
        .unwrap()
        .iter()
        .find(|tag| tag["name"] == "v1")
        .unwrap_or_else(|| panic!("{tags}"));
    assert_eq!(tag["message"], "Release notes", "{tags}");

    // Repository descriptions take 2,000 bytes on creation and update, JSON and forms alike.
    let at_limit = "d".repeat(2_000);
    let over_limit = "d".repeat(2_001);
    let (status, value) = alice
        .api(
            "POST",
            "/api/repos",
            json!({"name": "described", "description": over_limit.as_str()}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");
    alice
        .expect(
            "POST",
            "/api/repos",
            json!({"name": "described", "description": at_limit.as_str()}),
            StatusCode::CREATED,
        )
        .await;
    let (status, value) = alice
        .api(
            "PATCH",
            "/api/repos/alice/proj",
            json!({"is_private": false, "description": over_limit.as_str()}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"], "validation_error", "{value}");
    alice
        .expect(
            "PATCH",
            "/api/repos/alice/proj",
            json!({"is_private": false, "description": at_limit.as_str()}),
            StatusCode::OK,
        )
        .await;
    let (status, page) = alice
        .form(
            "/new",
            &[
                ("name", "described-form"),
                ("description", over_limit.as_str()),
                ("visibility", "public"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let (status, page) = alice
        .form(
            "/new",
            &[
                ("name", "described-form"),
                ("description", at_limit.as_str()),
                ("visibility", "public"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    // A native repository form names the repository it was rendered for.
    let current = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    let proj_id = current["id"].as_i64().unwrap().to_string();
    assert_rejected_forms_return_to_get_pages(&alice, &proj_id).await;
    let (status, page) = alice
        .form(
            "/alice/proj/_forms/settings",
            &[
                ("repository_id", proj_id.as_str()),
                ("description", over_limit.as_str()),
                ("visibility", "public"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    let saved = alice
        .form_reply(
            "/alice/proj/_forms/settings",
            &[
                ("repository_id", proj_id.as_str()),
                ("description", at_limit.as_str()),
                ("visibility", "public"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
    assert_eq!(
        saved.location.as_deref(),
        Some("/alice/proj/settings?tab=general&saved=general")
    );
    let (status, page) = alice
        .page("/alice/proj/settings?tab=general&saved=general")
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("Settings saved."), "{page}");
    // Discussion lists are paged, oldest first: three items read two per page.
    for body in ["Second comment", "Third comment"] {
        let (status, value) = alice.api("POST", &comments, json!({"body": body})).await;
        assert!(status.is_success(), "{value}");
    }
    assert_pages(
        &alice,
        &comments,
        &["First comment", "Second comment", "Third comment"],
    )
    .await;
    let review_list = format!("/api/repos/alice/proj/pulls/{pull_number}/reviews");
    let pull_bodies = ["Pull first", "Pull second", "Pull third"];
    for body in pull_bodies {
        let (status, value) = alice
            .api("POST", &pull_comments, json!({"body": body}))
            .await;
        assert!(status.is_success(), "{value}");
    }
    let mut review_bodies = bodies_on(&alice, &review_list).await;
    for body in ["Review first", "Review second", "Review third"] {
        let (status, value) = alice.api("POST", &review_list, json!({"body": body})).await;
        assert!(status.is_success(), "{value}");
        review_bodies.push(body.to_owned());
    }
    assert_pages(&alice, &pull_comments, &pull_bodies).await;
    assert_pages(&alice, &review_list, &review_bodies).await;
    assert_form_comments_land_on_their_page(&alice, &main).await;

    // A repository update returns the row GET returns, fork ancestry included.
    let bob_client = Client::login(&router, f.pool(), bob_id).await;
    assert_issue_notification_links(&f, &alice, &bob_client).await;
    assert_pull_notification_link(&alice, &bob_client, &main).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        assert_large_comment_socket(&alice, &bob_client),
    )
    .await
    .expect("notification socket completed");
    assert_native_list_omits_bodies(&alice).await;
    assert_native_list_search(&alice).await;
    assert_notification_actor_batch(&f, &alice, alice_id).await;
    db::repositories::access::revoke_repo_access(f.pool(), bob_id, proj_id.parse().unwrap())
        .await
        .unwrap();
    assert_comment_paths_are_bounded(&f, &bob_client, bob_id, &pull).await;
    let (status, forked) = bob_client
        .api("POST", "/api/repos/alice/proj/forks", json!({}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{forked}");
    let fork_path = format!(
        "/api/repos/{}",
        forked["repo"]["full_name"].as_str().unwrap()
    );
    let patched = bob_client
        .expect(
            "PATCH",
            &fork_path,
            json!({"is_private": false, "description": "Changed"}),
            StatusCode::OK,
        )
        .await;
    let fetched = bob_client
        .expect("GET", &fork_path, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(
        patched["forked_from"]["full_name"], "alice/proj",
        "{patched}"
    );
    assert_eq!(patched["updated_at"], fetched["updated_at"], "{patched}");

    assert_merge_methods(&alice, &bob_client).await;

    // An archived repository is not reported as writable, even to its owner.
    let proj = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    let permissions = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/permissions",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(permissions["can_write"], true, "{permissions}");
    f.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[proj["id"].as_i64().unwrap()],
    )
    .await;
    let archived = alice
        .expect(
            "GET",
            "/api/repos/alice/proj/permissions",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(archived["can_write"], false, "{archived}");
    f.finish().await;
}

/// Merge methods through the JSON API and the settings form. An administrator sets them, the
/// last allowed method cannot be turned off, and a non-administrator gets the same refusal as
/// for the other settings.
async fn assert_merge_methods(alice: &Client, bob: &Client) {
    let current = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    for name in [
        "allow_merge_commit",
        "allow_squash_merge",
        "allow_rebase_merge",
    ] {
        assert_eq!(current[name], true, "{current}");
    }
    let repository_id = current["id"].as_i64().unwrap().to_string();

    // A method left out keeps its value, and a disabled one shows in GET.
    let patched = alice
        .expect(
            "PATCH",
            "/api/repos/alice/proj",
            json!({"is_private": false, "description": "Merge methods", "allow_squash_merge": false}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(patched["allow_merge_commit"], true, "{patched}");
    assert_eq!(patched["allow_squash_merge"], false, "{patched}");
    assert_eq!(patched["allow_rebase_merge"], true, "{patched}");

    // Turning off the last allowed method is refused, and nothing else in the request applies.
    let (status, refused) = alice
        .api(
            "PATCH",
            "/api/repos/alice/proj",
            json!({"is_private": false, "description": "Refused", "allow_merge_commit": false, "allow_rebase_merge": false}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "validation_error", "{refused}");
    assert_eq!(
        refused["message"], "At least one merge method must stay allowed",
        "{refused}"
    );
    let unchanged = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    assert_eq!(unchanged["allow_merge_commit"], true, "{unchanged}");
    assert_eq!(unchanged["allow_squash_merge"], false, "{unchanged}");
    assert_eq!(unchanged["allow_rebase_merge"], true, "{unchanged}");
    assert_eq!(unchanged["description"], "Merge methods", "{unchanged}");

    // A non-administrator gets the refusal that the other settings give.
    let (plain_status, plain) = bob
        .api(
            "PATCH",
            "/api/repos/alice/proj",
            json!({"is_private": false, "description": "Merge methods"}),
        )
        .await;
    let (status, merge) = bob
        .api(
            "PATCH",
            "/api/repos/alice/proj",
            json!({"is_private": false, "description": "Merge methods", "allow_merge_commit": false}),
        )
        .await;
    assert_eq!(status, plain_status, "{merge}");
    assert_eq!(merge, plain);

    // The settings form works without JavaScript. A refused save says why and keeps the
    // stored methods; a saved one redirects back to the settings page.
    let form = "/alice/proj/_forms/merge-methods";
    let (status, page) = alice
        .form(form, &[("repository_id", repository_id.as_str())])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(
        page.contains("At least one merge method must stay allowed"),
        "{page}"
    );
    let kept = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    assert_eq!(kept["allow_merge_commit"], true, "{kept}");
    assert_eq!(kept["allow_squash_merge"], false, "{kept}");
    assert_eq!(kept["allow_rebase_merge"], true, "{kept}");

    // The retry form of the refused save carries the three checkboxes. Every box was unchecked,
    // so it posts only its hidden fields, and ticking one of them saves.
    let retry = form_at(&page, form);
    for name in [
        "allow_merge_commit",
        "allow_squash_merge",
        "allow_rebase_merge",
    ] {
        assert!(retry.contains(&format!("name=\"{name}\"")), "{retry}");
    }
    let mut fields = posted_fields(retry);
    assert_eq!(
        fields,
        [("repository_id".to_owned(), repository_id.clone())],
        "{retry}"
    );
    fields.push(("allow_merge_commit".to_owned(), "on".to_owned()));
    fields.push(("confirm".to_owned(), "yes".to_owned()));
    let retry_pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let (status, page) = alice.form(form, &retry_pairs).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let saved = alice
        .expect("GET", "/api/repos/alice/proj", Value::Null, StatusCode::OK)
        .await;
    assert_eq!(saved["allow_merge_commit"], true, "{saved}");
    assert_eq!(saved["allow_squash_merge"], false, "{saved}");
    assert_eq!(saved["allow_rebase_merge"], false, "{saved}");

    // Only administrators submit the form.
    let (status, page) = bob
        .form(
            form,
            &[
                ("repository_id", repository_id.as_str()),
                ("allow_merge_commit", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
}

/// The form that posts to `action` in a rendered page.
fn form_at<'a>(page: &'a str, action: &str) -> &'a str {
    page.split("<form")
        .skip(1)
        .find(|form| {
            form.split('>')
                .next()
                .unwrap()
                .contains(&format!("action=\"{action}\""))
        })
        .expect("a form with this action")
        .split("</form>")
        .next()
        .unwrap()
}

/// The fields a browser posts from a form: its hidden inputs and its checked boxes. The CSRF
/// token is left out, because `Client::form` sends the session's token itself.
fn posted_fields(form: &str) -> Vec<(String, String)> {
    form.split("<input")
        .skip(1)
        .filter_map(|input| {
            let tag = input.split('>').next().unwrap();
            let attribute = |name: &str| {
                let prefix = format!("{name}=\"");
                tag.split_whitespace()
                    .find_map(|token| token.strip_prefix(prefix.as_str()))
                    .map(|value| value.trim_end_matches('"').to_owned())
            };
            let checked = tag
                .split_whitespace()
                .any(|token| token == "checked" || token.starts_with("checked="));
            let posted = match attribute("type")?.as_str() {
                "hidden" => true,
                "checkbox" => checked,
                _ => false,
            };
            let name = attribute("name")?;
            (posted && name != "csrf_token")
                .then(|| (name, attribute("value").unwrap_or_else(|| "on".to_owned())))
        })
        .collect()
}

async fn assert_rejected_forms_return_to_get_pages(alice: &Client, repository_id: &str) {
    for (action, suffix) in [
        ("create-branch", "branches"),
        ("delete-branch", "branches"),
        ("create-tag", "tags"),
        ("delete-tag", "tags"),
        ("settings", "settings"),
        ("rename", "settings"),
        ("delete", "settings"),
        ("collaborator", "settings"),
        ("remove-collaborator", "settings"),
        ("label", "labels"),
        ("delete-label", "labels"),
    ] {
        let (status, page) = alice
            .form(
                &format!("/alice/proj/_forms/{action}"),
                &[("repository_id", repository_id)],
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{action}: {page}");
        let return_to = switch_target(&page);
        assert_eq!(return_to, format!("/alice/proj/{suffix}"), "{action}");
        let theme = alice
            .form_reply(
                "/_forms/theme",
                &[("theme", "dark"), ("return_to", return_to)],
            )
            .await;
        assert_eq!(theme.status, StatusCode::SEE_OTHER, "{}", theme.text());
        assert_eq!(theme.location.as_deref(), Some(return_to));
        let (status, _) = alice.page(return_to).await;
        assert_eq!(status, StatusCode::OK, "{action}: {return_to}");
    }
    // A forged token fails before the form is read, and that failure page links back the same way.
    let (status, page) = alice
        .form(
            "/alice/proj/_forms/create-tag",
            &[("repository_id", repository_id), ("csrf_token", "forged")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert_eq!(switch_target(&page), "/alice/proj/tags");
}

/// The path that the appearance switch on a rendered page submits.
fn switch_target(page: &str) -> &str {
    page.split("name=\"return_to\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
}

async fn assert_comment_paths_are_bounded(
    f: &Database,
    reader: &Client,
    reader_id: i64,
    pull: &Value,
) {
    let path = format!("/api/repos/alice/proj/pulls/{}/comments", pull["number"]);
    let pr_id = pull["id"].as_i64().unwrap();
    let before = db::pull_requests::reviews::count_pull_request_comments(f.pool(), pr_id)
        .await
        .unwrap();
    for oversized in ["x".repeat(4097), "é".repeat(2049), "x".repeat(1024 * 1024)] {
        let rejected = reader
            .expect(
                "POST",
                &path,
                json!({"body": "short", "path": oversized}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(rejected["error"], "validation_error");
    }
    assert_eq!(
        db::pull_requests::reviews::count_pull_request_comments(f.pool(), pr_id)
            .await
            .unwrap(),
        before
    );
    for accepted in ["x".repeat(4096), "é".repeat(2048)] {
        // These paths pass the length bound, then the diff check refuses them: this pull request
        // does not change such a file. The root is stored through the data layer instead.
        let refused = reader
            .expect(
                "POST",
                &path,
                json!({"body": "At limit", "path": accepted, "line": 1, "side": "RIGHT"}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(refused["error"], "validation_error");
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("this pull request changes"),
            "{refused}"
        );
        let parent = db::pull_requests::reviews::add_pull_request_comment(
            f.pool(),
            db::pull_requests::reviews::NewPullRequestComment {
                pr_id,
                author_id: reader_id,
                body: "At limit",
                expected_revisions: None,
                path: Some(&accepted),
                line: Some(1),
                side: Some("RIGHT"),
                commit_sha: None,
                in_reply_to_comment_id: None,
            },
        )
        .await
        .unwrap();
        let reply = reader
            .expect(
                "POST",
                &path,
                json!({"body": "Inherited at limit", "in_reply_to_comment_id": parent.id}),
                StatusCode::CREATED,
            )
            .await;
        assert_eq!(reply["path"], accepted);
        assert_eq!(reply["line"], 1);
        let listed = reader
            .expect("GET", &path, Value::Null, StatusCode::OK)
            .await;
        assert_eq!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == reply["id"])
                .unwrap()["path"],
            accepted
        );
    }
    // Paths over the budget are stored as given. Reads leave them out,
    // and a reply to one is refused.
    for oversized_path in ["x".repeat(1024 * 1024), "é".repeat(2049)] {
        let oversized = db::pull_requests::reviews::add_pull_request_comment(
            f.pool(),
            db::pull_requests::reviews::NewPullRequestComment {
                pr_id,
                author_id: reader_id,
                body: "Oversized path",
                expected_revisions: None,
                path: Some(&oversized_path),
                line: Some(1),
                side: Some("RIGHT"),
                commit_sha: pull["head_sha"].as_str(),
                in_reply_to_comment_id: None,
            },
        )
        .await
        .unwrap();
        let count = db::pull_requests::reviews::count_pull_request_comments(f.pool(), pr_id)
            .await
            .unwrap();
        let rejected = reader
            .expect(
                "POST",
                &path,
                json!({"body": "Do not copy", "in_reply_to_comment_id": oversized.id}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(rejected["error"], "validation_error");
        assert_eq!(
            db::pull_requests::reviews::count_pull_request_comments(f.pool(), pr_id)
                .await
                .unwrap(),
            count
        );
        let listed = reader
            .expect("GET", &path, Value::Null, StatusCode::OK)
            .await;
        let row = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == oversized.id)
            .unwrap();
        assert!(row["path"].is_null());
        assert_eq!(row["body"], "Oversized path");
        let (status, page) = reader
            .page(&format!("/alice/proj/pulls/{}", pull["number"]))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Oversized path"));
        assert!(!page.contains(&oversized_path));
        let stored =
            db::pull_requests::reviews::get_pull_request_comment_by_id(f.pool(), oversized.id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(stored.path.as_deref(), Some(oversized_path.as_str()));
    }
}

/// The bodies of a discussion list, in the order it returns them.
async fn bodies_on(client: &Client, path: &str) -> Vec<String> {
    let list = client
        .expect("GET", path, Value::Null, StatusCode::OK)
        .await;
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["body"].as_str().unwrap().to_owned())
        .collect()
}

/// Reads a discussion list two items per page. The pages continue in order through
/// every item, and out-of-range pages are refused.
async fn assert_pages<S: AsRef<str>>(client: &Client, path: &str, bodies: &[S]) {
    let mut seen = Vec::new();
    for page in 1..=bodies.len().div_ceil(2) {
        let value = client
            .expect(
                "GET",
                &format!("{path}?page={page}&per_page=2"),
                Value::Null,
                StatusCode::OK,
            )
            .await;
        let items = value.as_array().unwrap();
        assert!(items.len() <= 2, "{value}");
        seen.extend(
            items
                .iter()
                .map(|item| item["body"].as_str().unwrap().to_owned()),
        );
    }
    let expected: Vec<&str> = bodies.iter().map(AsRef::as_ref).collect();
    assert_eq!(seen, expected);
    for query in ["page=0", "page=1001"] {
        let (status, value) = client
            .api("GET", &format!("{path}?{query}"), Value::Null)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"], "validation_error", "{value}");
    }
}

/// A comment saved through the native form redirects to the discussion page that
/// holds it. The first comment is on page 1, so its redirect leaves `page` out. The
/// 31st is on page 2, and following its redirect shows its body.
async fn assert_form_comments_land_on_their_page(alice: &Client, main: &str) {
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Long discussion", "body": "Body"}),
            StatusCode::CREATED,
        )
        .await;
    let issue_number = issue["number"].as_i64().unwrap();
    alice.branch("long", main).await;
    let tip = alice.save("long", "long.txt", main).await;
    let changed = alice.expect(
        "PUT",
        "/api/repos/alice/proj/contents/README.md",
        json!({"branch": "long", "expected_revision": tip, "message": "Review escaped changes",
            "content": base64::engine::general_purpose::STANDARD.encode("<script>patch()</script>\n++addition\n")}),
        StatusCode::OK,
    ).await;
    let pull = alice.open_pull("Long discussion", "long", "main").await;
    let pull_number = pull["number"].as_i64().unwrap();
    for path in [
        format!("/alice/proj/pulls/{pull_number}/diff"),
        format!(
            "/alice/proj/commit/{}",
            changed["commit"]["sha"].as_str().unwrap()
        ),
    ] {
        let (status, page) = alice.page(&path).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let page = page
            .split("<!--")
            .enumerate()
            .map(|(index, part)| {
                if index == 0 {
                    part
                } else {
                    part.split_once("-->").unwrap().1
                }
            })
            .collect::<String>();
        assert!(page.contains("class=\"patch-line patch-hunk\""), "{path}");
        for (class, text) in [
            ("patch-line patch-deletion", "-README.md\n"),
            (
                "patch-line patch-addition",
                "+&#60;script&#62;patch()&#60;/script&#62;\n",
            ),
            ("patch-line patch-addition", "+++addition\n"),
        ] {
            assert!(
                page.split("<span ").skip(1).any(|span| {
                    let (attributes, body) = span.split_once('>').unwrap();
                    attributes.contains(&format!("class=\"{class}\""))
                        && body.split("</span>").next() == Some(text)
                }),
                "{path}: missing {class}: {text}\n{page}"
            );
        }
        assert!(!page.contains("<script>patch()</script>"), "{path}");
        // Git's header lines are replaced by the file's header row. The page also carries the
        // raw patch in its serialized model, so only rendered patch rows are checked here.
        assert!(
            !page.split("<span ").skip(1).any(|span| span
                .split_once('>')
                .unwrap()
                .1
                .starts_with("diff --git ")),
            "{path}: git's header line is shown as a patch row"
        );
        assert!(
            !page.split("<span ").skip(1).any(|span| span
                .split_once('>')
                .unwrap()
                .1
                .starts_with("--- a/README.md")),
            "{path}: git's header line is shown as a patch row"
        );
    }
    let discussions = [
        (
            format!("/alice/proj/issues/{issue_number}/comments"),
            format!("/api/repos/alice/proj/issues/{issue_number}/comments"),
        ),
        (
            format!("/alice/proj/pulls/{pull_number}/comments"),
            format!("/api/repos/alice/proj/pulls/{pull_number}/comments"),
        ),
    ];
    for (form_path, json_path) in discussions {
        let first = alice
            .form_reply(&form_path, &[("body", "First from the form")])
            .await;
        assert_eq!(first.status, StatusCode::SEE_OTHER, "{}", first.text());
        let location = first.location.unwrap();
        assert!(!location.contains("page="), "{location}");
        let (target, fragment) = location.split_once('#').unwrap();
        let link_body = format!("[Earlier](#{fragment}) [Prefixed](#user-content-details) [Heading](#details)\n## Details");
        let (status, value) = alice
            .api("POST", &json_path, json!({"body": link_body}))
            .await;
        assert!(status.is_success(), "{value}");
        let (status, page) = alice.page(target).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains(&format!("id=\"{fragment}\"")), "{page}");
        assert!(page.contains(&format!("href=\"#{fragment}\"")), "{page}");
        assert!(page.contains("id=\"user-content-details\""), "{page}");
        // The two written links; a comment's headings have no permalink of their own.
        assert_eq!(page.matches("href=\"#user-content-details\"").count(), 2);
        assert!(!page.contains("heading-permalink"));
        assert!(!page.contains("href=\"#user-content-user-content-"));
        for number in 3..=30 {
            let (status, value) = alice
                .api(
                    "POST",
                    &json_path,
                    json!({"body": format!("Comment {number}")}),
                )
                .await;
            assert!(status.is_success(), "{value}");
        }
        let last = alice
            .form_reply(&form_path, &[("body", "Thirty-first from the form")])
            .await;
        assert_eq!(last.status, StatusCode::SEE_OTHER, "{}", last.text());
        let location = last.location.unwrap();
        let (target, fragment) = location.split_once('#').unwrap();
        assert!(target.ends_with("?page=2"), "{location}");
        assert!(fragment.starts_with("comment-"), "{location}");
        let (status, page) = alice.page(target).await;
        assert_eq!(status, StatusCode::OK, "{location}");
        assert!(page.contains("Thirty-first from the form"), "{location}");
        let comment_id = fragment.strip_prefix("comment-").unwrap();
        let preview = alice
            .form_reply(
                &format!("{form_path}/{comment_id}/edit"),
                &[
                    ("body", "Preview only on page two"),
                    ("intent", "preview"),
                    ("expected_body", "\"Thirty-first from the form\""),
                ],
            )
            .await;
        assert_eq!(preview.status, StatusCode::OK, "{}", preview.text());
        let html = preview.text();
        let edit_form = html
            .split(&format!("id=\"edit-comment-{comment_id}\""))
            .nth(1)
            .unwrap();
        assert!(edit_form.contains("Preview only on page two"));
        let composer = html
            .split("collaboration-comment-form collaboration-composer")
            .nth(1)
            .unwrap();
        let composer = composer.split("</form>").next().unwrap();
        assert!(!composer.contains("Preview only on page two"));
        assert_draft_acknowledgment(
            alice,
            &format!("{form_path}/{comment_id}/edit"),
            "Thirty-first from the form",
        )
        .await;
    }
}

async fn assert_draft_acknowledgment(client: &Client, path: &str, original: &str) {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    for intent in ["preview", "save"] {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf_token", &client.csrf)
            .append_pair("body", "Newer native submission B")
            .append_pair("expected_body", &serde_json::to_string(original).unwrap())
            .append_pair("draft_submission", "123-456.789")
            .append_pair("intent", intent)
            .finish();
        let response = client
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("cookie", format!("session_id={}", client.session))
                    .header("accept", codeyard::webui::bootstrap::MODEL_CONTENT_TYPE)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let accepted = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .any(|cookie| {
                cookie
                    .to_str()
                    .unwrap()
                    .starts_with("codeyard-draft-accepted=123-456.789;")
            });
        assert_eq!(accepted, intent == "save");
        if intent == "preview" {
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
                .await
                .unwrap();
            let model: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(model["chrome"]["submitted_form"], path);
            assert!(String::from_utf8_lossy(&bytes).contains("Newer native submission B"));
        } else {
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
        }
    }
}

async fn page_model(client: &Client, path: &str) -> (Vec<u8>, Value) {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let response = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Cookie", format!("session_id={}", client.session))
                .header("Accept", "application/vnd.codeyard.page+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    let model = serde_json::from_slice(&bytes).unwrap();
    (bytes, model)
}

/// The search narrows the native list and its state counts, and the tabs keep the search. The
/// list is an ordinary GET page, so the search text is read from the URL.
async fn assert_native_list_search(alice: &Client) {
    alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Zqneedle report", "body": "plain text"}),
            StatusCode::CREATED,
        )
        .await;
    let (_, model) = page_model(alice, "/alice/proj/issues?q=ZQNEEDLE").await;
    let list = &model["page"]["model"]["List"];
    assert_eq!(list["query"], "ZQNEEDLE");
    assert_eq!(list["counts"]["open"], 1);
    assert_eq!(list["counts"]["all"], 1);
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "Zqneedle report");
    let (_, model) = page_model(alice, "/alice/proj/issues?state=closed&q=zqneedle").await;
    let list = &model["page"]["model"]["List"];
    assert!(list["items"].as_array().unwrap().is_empty());
    assert_eq!(list["counts"]["open"], 1);
    // The box holds the filters as qualifiers: the state is read from the text, and the words left
    // over are the search.
    let (_, model) = page_model(
        alice,
        "/alice/proj/issues?q=is:issue%20state:closed%20zqneedle",
    )
    .await;
    let list = &model["page"]["model"]["List"];
    assert_eq!(list["state"], "closed");
    assert_eq!(list["query"], "zqneedle");
    assert!(list["items"].as_array().unwrap().is_empty());
    let (_, model) = page_model(
        alice,
        "/alice/proj/issues?q=is:issue%20state:open%20zqneedle",
    )
    .await;
    let list = &model["page"]["model"]["List"];
    assert_eq!(list["state"], "open");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
}

async fn assert_native_list_omits_bodies(alice: &Client) {
    let body = "\u{1}".repeat(65_536);
    let mut last = Value::Null;
    for number in 1..=43 {
        last = alice
            .expect(
                "POST",
                "/api/repos/alice/proj/issues",
                json!({"title": format!("Large issue {number}"), "body": body}),
                StatusCode::CREATED,
            )
            .await;
    }
    let (bytes, model) = page_model(alice, "/alice/proj/issues").await;
    assert!(
        bytes.len() < 16 * 1024 * 1024,
        "list exceeds navigation limit"
    );
    let items = model["page"]["model"]["List"]["items"].as_array().unwrap();
    assert!(items.len() >= 43);
    assert!(items.iter().all(|item| item.get("body").is_none()));
    let envelope: codeyard::webui::app::PageEnvelope = serde_json::from_slice(&bytes).unwrap();
    let codeyard::webui::app::Page::Collaboration(page) = envelope.page else {
        panic!("collaboration page")
    };
    let codeyard::webui::pages::repos::issues::Page::List { items, .. } = *page else {
        panic!("issue list")
    };
    assert!(items.iter().all(|item| item.body.is_empty()));
    let number = last["number"].as_i64().unwrap();
    let (_, detail) = page_model(alice, &format!("/alice/proj/issues/{number}")).await;
    assert_eq!(detail["page"]["model"]["Detail"]["item"]["body"], body);
    let api = alice
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/issues/{number}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(api["body"], body);
    // The API list still returns full descriptions, unlike the native projection.
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let response = alice
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/repos/alice/proj/issues")
                .header("Cookie", format!("session_id={}", alice.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let list: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(list[0]["body"], body);
}

async fn assert_issue_notification_links(f: &Database, alice: &Client, bob: &Client) {
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Notification discussion"}),
            StatusCode::CREATED,
        )
        .await;
    let number = issue["number"].as_i64().unwrap();
    let api_path = format!("/api/repos/alice/proj/issues/{number}/comments");
    let form_path = format!("/alice/proj/issues/{number}/comments");
    let first = bob
        .form_reply(&form_path, &[("body", "Notified first")])
        .await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    let mut locations = vec![first.location.unwrap()];
    for index in 2..=30 {
        alice
            .expect(
                "POST",
                &api_path,
                json!({"body": format!("Comment {index}")}),
                StatusCode::CREATED,
            )
            .await;
    }
    let next = bob
        .form_reply(&form_path, &[("body", "Notified thirty-first")])
        .await;
    assert_eq!(next.status, StatusCode::SEE_OTHER);
    locations.push(next.location.unwrap());
    let last = bob
        .expect(
            "POST",
            &api_path,
            json!({"body": "Notified through JSON"}),
            StatusCode::CREATED,
        )
        .await;
    locations.push(format!(
        "/alice/proj/issues/{number}?page=2#comment-{}",
        last["id"]
    ));
    let inbox = alice
        .expect("GET", "/api/notifications", Value::Null, StatusCode::OK)
        .await;
    let (_, native_inbox) = alice.page("/notifications").await;
    // Each row shows the picture of the account that caused it.
    assert!(native_inbox.contains("src=\"/avatars/bob\""));
    for (index, location) in locations.iter().enumerate() {
        let notification = inbox["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                location.ends_with(&format!("#comment-{}", item["metadata"]["comment_id"]))
            })
            .expect("notification names the saved comment");
        let stable = notification["link"].as_str().unwrap();
        let id = notification["metadata"]["comment_id"].as_i64().unwrap();
        assert_eq!(
            stable,
            format!("/alice/proj/issues/{number}?comment={id}#comment-{id}")
        );
        let followed = alice
            .exchange(
                "GET",
                stable.split('#').next().unwrap(),
                "text/html",
                Vec::new(),
            )
            .await;
        assert_eq!(followed.status, StatusCode::OK);
        assert!(followed.text().contains(&format!("id=\"comment-{id}\"")));
        let (target, fragment) = location.split_once('#').unwrap();
        assert_eq!(target.contains("?page=2"), index > 0);
        assert_eq!(
            fragment,
            format!("comment-{}", notification["metadata"]["comment_id"])
        );
        assert!(native_inbox.contains(stable));
        let (status, page) = alice.page(target).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains(notification["body"].as_str().unwrap()));
        assert!(page.contains(&format!("id=\"{fragment}\"")));
    }
    // Deleting preceding comments moves the target, but the saved notification still reaches it.
    let first_id: i64 = locations[0]
        .rsplit("comment-")
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let second_id = db::issues::list_issue_comments(f.pool(), issue["id"].as_i64().unwrap(), 2, 0)
        .await
        .unwrap()[1]
        .comment
        .id;
    for id in [first_id, second_id] {
        let removed = alice
            .form_reply(
                &format!("/alice/proj/issues/{number}/comments/{id}/delete"),
                &[],
            )
            .await;
        assert_eq!(removed.status, StatusCode::SEE_OTHER);
    }
    let id = last["id"].as_i64().unwrap();
    let stable = format!("/alice/proj/issues/{number}?comment={id}");
    let moved = alice
        .exchange("GET", &stable, "text/html", Vec::new())
        .await;
    assert_eq!(moved.status, StatusCode::OK);
    assert!(moved.text().contains(&format!("id=\"comment-{id}\"")));
    let (status, model) = alice.model(&stable).await;
    assert_eq!(status, StatusCode::OK);
    assert!(serde_json::to_string(&model)
        .unwrap()
        .contains("Notified through JSON"));
    let (status, _) = alice
        .page(&format!("/alice/proj/issues/999999?comment={id}"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Positions follow (created_at, id): equal timestamps fall back to id, and an earlier
    // timestamp moves a comment to the front.
    f.execute(
        "UPDATE issue_comments SET created_at='2020-01-01 00:00:00' WHERE issue_id=$1",
        &[issue["id"].as_i64().unwrap()],
    )
    .await;
    let ordered = db::issues::list_issue_comments(f.pool(), issue["id"].as_i64().unwrap(), 100, 0)
        .await
        .unwrap();
    for (index, row) in ordered.iter().enumerate() {
        assert_eq!(
            db::issues::issue_comment_position(f.pool(), row.comment.id)
                .await
                .unwrap(),
            (index + 1) as i64
        );
    }
    let id = last["id"].as_i64().unwrap();
    f.execute(
        "UPDATE issue_comments SET created_at='2019-01-01 00:00:00' WHERE id=$1",
        &[id],
    )
    .await;
    assert_eq!(
        db::issues::issue_comment_position(f.pool(), id)
            .await
            .unwrap(),
        1
    );
}

/// A pull request comment notification links to the discussion page that holds its
/// comment, as an issue comment notification does.
async fn assert_pull_notification_link(alice: &Client, bob: &Client, main: &str) {
    alice.branch("notify", main).await;
    alice.save("notify", "notify.txt", main).await;
    let pull = alice.open_pull("Notification pull", "notify", "main").await;
    let number = pull["number"].as_i64().unwrap();
    let api_path = format!("/api/repos/alice/proj/pulls/{number}/comments");
    let form_path = format!("/alice/proj/pulls/{number}/comments");
    for index in 1..=30 {
        let (status, value) = bob
            .api(
                "POST",
                &api_path,
                json!({"body": format!("Pull comment {index}")}),
            )
            .await;
        assert!(status.is_success(), "{value}");
    }
    let reply = bob
        .form_reply(&form_path, &[("body", "Pull thirty-first")])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let location = reply.location.unwrap();
    let (target, fragment) = location.split_once('#').unwrap();
    assert!(target.ends_with("?page=2"), "{location}");
    let inbox = alice
        .expect("GET", "/api/notifications", Value::Null, StatusCode::OK)
        .await;
    // The stored link names the comment; following it redirects to the comment's current page.
    let base = target.strip_suffix("?page=2").unwrap();
    let comment_id = fragment.trim_start_matches("comment-");
    let stored = format!("{base}?comment={comment_id}#{fragment}");
    let notification = inbox["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["link"] == stored)
        .expect("pull request notification names the comment");
    assert_eq!(
        fragment,
        format!("comment-{}", notification["metadata"]["comment_id"])
    );
    let (status, page) = alice.page(target).await;
    assert_eq!(status, StatusCode::OK, "{location}");
    assert!(page.contains("Pull thirty-first"), "{location}");
    assert!(page.contains(&format!("id=\"{fragment}\"")), "{location}");
}

async fn assert_large_comment_socket(alice: &Client, bob: &Client) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = alice.router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("GET /api/notifications/ws HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nCookie: session_id={}\r\n\r\n", alice.session).as_bytes()).await.unwrap();
    let mut handshake = Vec::new();
    while !handshake.ends_with(b"\r\n\r\n") {
        handshake.push(socket.read_u8().await.unwrap());
        assert!(handshake.len() < 8192);
    }
    assert!(handshake.starts_with(b"HTTP/1.1 101"));
    // A masked empty ping confirms the upgraded handler is subscribed before posting.
    socket.write_all(&[0x89, 0x80, 0, 0, 0, 0]).await.unwrap();
    assert_eq!(socket.read_u16().await.unwrap(), 0x8a00);
    let issue = alice
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title": "Socket discussion"}),
            StatusCode::CREATED,
        )
        .await;
    for body in [
        "x".repeat(65_536),
        "\u{1}".repeat(16_384),
        "small follow-up".into(),
    ] {
        let comment = bob
            .expect(
                "POST",
                &format!("/api/repos/alice/proj/issues/{}/comments", issue["number"]),
                json!({"body": body}),
                StatusCode::CREATED,
            )
            .await;
        assert_eq!(
            socket.read_u8().await.unwrap(),
            0x81,
            "socket must send text, not close"
        );
        let length = socket.read_u8().await.unwrap();
        assert_eq!(length & 0x80, 0, "server frames are not masked");
        let length = match length {
            126 => socket.read_u16().await.unwrap() as usize,
            127 => socket.read_u64().await.unwrap() as usize,
            value => value as usize,
        };
        assert!(length <= 60 * 1024);
        let mut payload = vec![0; length];
        socket.read_exact(&mut payload).await.unwrap();
        let event: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(event["type"], "created");
        if body == "small follow-up" {
            assert_eq!(event["body"], body);
        } else {
            assert!(event["body"].is_null());
        }
        assert!(event["link"]
            .as_str()
            .unwrap()
            .ends_with(&format!("#comment-{}", comment["id"])));
        let inbox = alice
            .expect("GET", "/api/notifications", Value::Null, StatusCode::OK)
            .await;
        let saved = inbox["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == event["notification_id"])
            .unwrap();
        assert_eq!(saved["body"], body);
        assert_eq!(saved["link"], event["link"]);
    }
    drop(socket);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[derive(Clone, Default)]
struct QueryLog(Arc<std::sync::Mutex<Vec<String>>>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for QueryLog {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            event.record(
                &mut |field: &tracing::field::Field, value: &dyn std::fmt::Debug| {
                    if field.name() == "db.statement" || field.name() == "sql" {
                        self.0.lock().unwrap().push(format!("{value:?}"));
                    }
                },
            );
        }
    }
}

async fn assert_notification_actor_batch(f: &Database, alice: &Client, recipient: i64) {
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;
    let mut expected = std::collections::HashMap::new();
    for index in 0..100 {
        let name = format!("notification_actor_{index}");
        let actor = f.actor(&name).await;
        let notification = db::notifications::records::create_notification(
            f.pool(),
            db::notifications::records::NewNotification {
                user_id: recipient,
                actor_id: Some(actor),
                repo_id: None,
                kind: "test",
                title: "Actor projection",
                body: None,
                link: None,
                metadata: &Value::Null,
            },
        )
        .await
        .unwrap();
        expected.insert(notification.id, (actor, name));
    }
    alice
        .expect(
            "POST",
            "/api/notifications/read-all",
            json!({}),
            StatusCode::OK,
        )
        .await;
    let queries = QueryLog::default();
    let inbox = alice
        .expect(
            "GET",
            "/api/notifications?limit=100",
            Value::Null,
            StatusCode::OK,
        )
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await;
    let items = inbox["notifications"].as_array().unwrap();
    assert_eq!(items.len(), 100);
    assert_eq!(inbox["unread_count"], 0);
    for item in items {
        let (id, name) = &expected[&item["id"].as_i64().unwrap()];
        assert_eq!(item["actor"]["id"], *id);
        assert_eq!(item["actor"]["username"], *name);
        assert!(item["read_at"].is_string());
    }
    // PostgreSQL emits query events on this request future; SQLite runs its SQL on a worker.
    if matches!(f.pool(), db::DbPool::Postgres(_)) {
        let queries = queries.0.lock().unwrap();
        let batches = queries
            .iter()
            .filter(|sql| sql.contains("FROM users WHERE id = ANY"))
            .count();
        assert_eq!(batches, 1, "actor batch reads in {queries:?}");
    }
}

#[tokio::test]
async fn sqlite_collaboration_inputs_match_on_json_and_forms() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_collaboration_inputs_match_on_json_and_forms() {
    contract(true).await;
}
