use super::github::exchange;
use crate::support::{
    database::Database,
    fake_webui_bundle as bundle,
    http_client::{visible_text, Client},
};
use axum::{http::StatusCode, Router};
use base64::Engine;
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
    repositories::lifecycle,
};
use serde_json::{json, Value};

async fn call(
    router: &Router,
    operation: &str,
    method: &str,
    url: &str,
    token: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    eprintln!("{operation} {method} {url}: expecting {expected}, input={body}");
    let (status, _, body) = exchange(router, operation, method, url, Some(token), body).await;
    assert_eq!(status, expected, "{operation}: {body}");
    body
}

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
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
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let mut credentials = Vec::new();
    for (id, client) in [(alice_id, &alice), (bob_id, &bob)] {
        let token = tokens::create(
            f.pool(),
            id,
            Some(&client.session),
            TokenInput {
                name: "conversion",
                scope: "write",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let (owner, reader) = (credentials[0].as_str(), credentials[1].as_str());
    let base = alice.save("main", "README.md", "unborn").await;
    for branch in ["other", "feature", "locked", "closed"] {
        alice.branch(branch, &base).await;
        alice.save(branch, &format!("{branch}.txt"), &base).await;
    }
    // Reserve a PR comment ID that collides with the source table's first ID.
    let other = alice.open_pull("Unrelated", "other", "main").await;
    let other_number = other["number"].as_i64().unwrap();
    let prior = alice
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{other_number}/comments"),
            json!({"body":"Unrelated comment"}),
            StatusCode::CREATED,
        )
        .await;
    let created = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/issues",
            json!({"title":"Original issue","body":"Original body @alice"}),
            StatusCode::CREATED,
        )
        .await;
    let number = created["number"].as_i64().unwrap() as i32;
    let source = db::issues::get_issue(f.pool(), repo.id, number)
        .await
        .unwrap()
        .unwrap();
    let original_created = source.created_at;
    alice.expect("PUT", &format!("/api/repos/alice/proj/issues/{number}"), json!({"expected_state":source.state,"expected_updated":source.updated_at.to_rfc3339(),"title":"Renamed source"}), StatusCode::OK).await;
    let old_comment = bob
        .expect(
            "POST",
            &format!("/api/repos/alice/proj/issues/{number}/comments"),
            json!({"body":"Existing discussion"}),
            StatusCode::CREATED,
        )
        .await;
    let old_id = old_comment["id"].as_i64().unwrap();
    assert_eq!(prior["id"], old_comment["id"]);
    let label = db::labels::create_label(
        f.pool(),
        alice_id,
        repo.id,
        "carried",
        "#123456",
        Some("Source label"),
    )
    .await
    .unwrap();
    db::labels::add_issue_label(f.pool(), alice_id, repo.id, source.id, label.id)
        .await
        .unwrap();
    db::assignees::assign_user(
        f.pool(),
        alice_id,
        repo.id,
        db::assignees::AssignedItem::Issue(source.id),
        alice_id,
    )
    .await
    .unwrap();
    let milestone =
        db::milestones::create_milestone(f.pool(), alice_id, repo.id, "Destination", None, None)
            .await
            .unwrap();
    db::milestones::set_issue_milestone(
        f.pool(),
        alice_id,
        repo.id,
        source.id,
        Some(milestone.number),
    )
    .await
    .unwrap();
    let (reaction, _) = db::reactions::create_reaction(
        f.pool(),
        alice_id,
        &db::users::accounts::AccountCredential::Session(&alice.session),
        db::reactions::Target {
            repo_id: repo.id,
            item_id: source.id,
            subject: db::reactions::Subject::IssueComment,
            subject_id: old_id,
        },
        "heart",
    )
    .await
    .unwrap();
    let (description_reaction, _) = db::reactions::create_reaction(
        f.pool(),
        alice_id,
        &db::users::accounts::AccountCredential::Session(&alice.session),
        db::reactions::Target {
            repo_id: repo.id,
            item_id: source.id,
            subject: db::reactions::Subject::Issue,
            subject_id: source.id,
        },
        "+1",
    )
    .await
    .unwrap();
    let notifications = call(
        &router,
        "activity/list-notifications-for-authenticated-user",
        "GET",
        "/api/v3/notifications?all=true",
        owner,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let thread = notifications
        .as_array()
        .unwrap()
        .iter()
        .find(|notice| {
            notice["subject"]["url"]
                .as_str()
                .is_some_and(|url| url.ends_with(&format!("/issues/{number}")))
        })
        .unwrap()["id"]
        .clone();
    let filler = db::issues::create_issue(
        f.pool(),
        repo.id,
        bob_id,
        "Later issue",
        "Keep the number counter",
    )
    .await
    .unwrap();
    let collection = "/api/v3/repos/alice/proj/pulls";
    let blocked_input = json!({"issue":filler.number,"head":"feature","base":"main"});
    let kind = db::issue_attributes::configure(
        f.pool(),
        repo.id,
        alice_id,
        &db::issue_attributes::Definition {
            id: None,
            expected_updated: None,
            kind: "type".into(),
            name: "Retained type".into(),
            description: None,
            data_type: None,
            visibility: "all".into(),
            is_enabled: true,
            color: None,
            options: Vec::new(),
        },
    )
    .await
    .unwrap();
    db::issue_attributes::set_type(f.pool(), repo.id, alice_id, filler.id, Some(kind))
        .await
        .unwrap();
    call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        blocked_input.clone(),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(
        db::issue_attributes::assigned_type(f.pool(), repo.id, filler.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        kind
    );
    db::issue_attributes::set_type(f.pool(), repo.id, alice_id, filler.id, None)
        .await
        .unwrap();
    db::relationships::change(
        f.pool(),
        alice_id,
        repo.id,
        filler.id,
        db::relationships::Mutation::Add {
            kind: db::relationships::Kind::Related,
            other: source.id,
            replace: false,
        },
    )
    .await
    .unwrap();
    call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        blocked_input.clone(),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(
        db::relationships::list(
            f.pool(),
            Some(alice_id),
            filler.id,
            db::relationships::Kind::Related,
            0,
            100
        )
        .await
        .unwrap()
        .len(),
        1
    );
    db::relationships::change(
        f.pool(),
        alice_id,
        repo.id,
        filler.id,
        db::relationships::Mutation::Remove {
            kind: db::relationships::Kind::Related,
            other: source.id,
        },
    )
    .await
    .unwrap();
    db::suggestions::set_enabled(f.pool(), repo.id, alice_id, &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), alice_id).await), true)
        .await
        .unwrap();
    let suggestion = db::suggestions::propose(
        f.pool(),
        repo.id,
        alice_id,
        &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), alice_id).await),
        filler.id,
        &db::suggestions::Proposal {
            action: db::suggestions::Action::CloseIssue,
            target_id: None,
            value: Value::Null,
            rationale: None,
            confidence: None,
        },
        None,
    )
    .await
    .unwrap();
    call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        blocked_input,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM issue_suggestions WHERE id=$1",
            &[suggestion.id]
        )
        .await,
        1
    );
    assert!(db::issues::get_issue(f.pool(), repo.id, filler.number)
        .await
        .unwrap()
        .is_some());
    db::pins::set_pin(
        f.pool(),
        alice_id,
        &db::users::accounts::AccountCredential::Session(&alice.session),
        repo.id,
        db::assignees::AssignedItem::Issue(source.id),
        old_id,
        true,
    )
    .await
    .unwrap();
    let original_pin = db::pins::get_pin(f.pool(), db::assignees::AssignedItem::Issue(source.id))
        .await
        .unwrap()
        .unwrap();
    let input = json!({"issue":number,"head":"feature","base":"main"});
    call(
        &router,
        "pulls/create",
        "POST",
        collection,
        reader,
        input.clone(),
        StatusCode::FORBIDDEN,
    )
    .await;
    assert!(db::issues::get_issue(f.pool(), repo.id, number)
        .await
        .unwrap()
        .is_some());
    let converted = call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        input.clone(),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(converted["number"], number);
    assert_eq!(converted["title"], "Renamed source");
    assert_eq!(converted["body"], "Original body @alice");
    assert_eq!(converted["user"]["login"], "bob");
    assert_eq!(converted["comments"], 1);
    assert_eq!(converted["review_comments"], 0);
    assert_eq!(converted["labels"][0]["id"], label.id);
    assert_eq!(converted["assignees"][0]["login"], "alice");
    assert_eq!(converted["milestone"]["number"], milestone.number);
    let pull = db::pull_requests::records::get_pull_request(f.pool(), repo.id, number)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pull.author_id, bob_id);
    assert_eq!(pull.head_publisher_id, Some(alice_id));
    assert_eq!(pull.created_at, original_created);
    assert_eq!(
        f.scalar(
            "SELECT source_issue_id FROM pull_requests WHERE id=$1",
            &[pull.id]
        )
        .await,
        source.id
    );
    assert!(db::issues::get_issue(f.pool(), repo.id, number)
        .await
        .unwrap()
        .is_none());
    let next = db::issues::create_issue(
        f.pool(),
        repo.id,
        bob_id,
        "Next",
        "Conversion allocates no discussion number",
    )
    .await
    .unwrap();
    assert_eq!(next.number, filler.number + 1);
    let new_id = db::pull_requests::reviews::converted_issue_comment(f.pool(), pull.id, old_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(new_id, old_id);
    let carried_pin =
        db::pins::get_pin(f.pool(), db::assignees::AssignedItem::PullRequest(pull.id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(carried_pin.comment_id, new_id);
    assert_eq!(carried_pin.pinned_by, original_pin.pinned_by);
    assert_eq!(carried_pin.pinned_at, original_pin.pinned_at);
    assert!(db::pull_requests::reviews::converted_issue_comment(
        f.pool(),
        other["id"].as_i64().unwrap(),
        old_id
    )
    .await
    .unwrap()
    .is_none());
    let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(f.pool(), new_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(comment.author_id, bob_id);
    assert_eq!(comment.body, "Existing discussion");
    assert_eq!(
        comment.created_at.to_rfc3339(),
        old_comment["created_at"].as_str().unwrap()
    );
    let reactions = db::reactions::list_reactions(
        f.pool(),
        db::reactions::Target {
            repo_id: repo.id,
            item_id: pull.id,
            subject: db::reactions::Subject::PullRequestComment,
            subject_id: new_id,
        },
        None,
        100,
        0,
    )
    .await
    .unwrap();
    assert_eq!(reactions[0].id, reaction.id);
    let reactions = db::reactions::list_reactions(
        f.pool(),
        db::reactions::Target {
            repo_id: repo.id,
            item_id: pull.id,
            subject: db::reactions::Subject::PullRequest,
            subject_id: pull.id,
        },
        None,
        100,
        0,
    )
    .await
    .unwrap();
    assert_eq!(reactions[0].id, description_reaction.id);
    let notifications = call(
        &router,
        "activity/list-notifications-for-authenticated-user",
        "GET",
        "/api/v3/notifications?all=true",
        owner,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let current = notifications
        .as_array()
        .unwrap()
        .iter()
        .find(|notice| notice["id"] == thread)
        .unwrap();
    assert_eq!(current["subject"]["type"], "PullRequest");
    assert_eq!(current["subject"]["title"], "Renamed source");
    let redirect = bob
        .exchange(
            "GET",
            &format!("/alice/proj/issues/{number}?comment={old_id}"),
            "text/html",
            Vec::new(),
        )
        .await;
    assert_eq!(redirect.status, StatusCode::SEE_OTHER);
    assert!(redirect.location.as_deref().unwrap().ends_with(&format!(
        "/pulls/{number}?comment={new_id}#comment-{new_id}"
    )));
    let html = bob.page_ok(&format!("/alice/proj/pulls/{number}")).await;
    let text = visible_text(&html);
    assert!(text.contains("Existing discussion"), "{text}");
    assert!(
        text.contains("converted this issue into a pull request"),
        "{text}"
    );
    // Simulate interruption between the committed transfer and retained-head publication.
    let reference = format!("refs/pull/{number}/head");
    assert!(tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repo.disk_path)
        .args(["update-ref", "-d"])
        .arg(&reference)
        .arg(&pull.head_sha)
        .status()
        .await
        .unwrap()
        .success());
    call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        input,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let repaired = codeyard::backend::repositories::store::repo_store(&repo, None)
        .commit_from_primary(&reference)
        .await
        .unwrap();
    assert_eq!(repaired.sha, pull.head_sha);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM pull_requests WHERE repo_id=$1",
            &[repo.id]
        )
        .await,
        2
    );
    // A duplicate head/base leaves the later source, its comments and the counter untouched.
    let rejected = call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        json!({"issue":filler.number,"head":"feature","base":"main"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(rejected["message"].is_string());
    assert!(db::issues::get_issue_by_id(f.pool(), filler.id)
        .await
        .unwrap()
        .is_some());
    for value in [0_i64, -1, i64::MAX] {
        call(
            &router,
            "pulls/create",
            "POST",
            collection,
            owner,
            json!({"issue":value,"head":"feature","base":"main"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    // A locked source is refused without silently unlocking or replacing the discussion.
    let locked = db::issues::create_issue(f.pool(), repo.id, bob_id, "Locked", "Retain the lock")
        .await
        .unwrap();
    f.execute(
        "UPDATE issues SET locked=TRUE,lock_reason='resolved' WHERE id=$1",
        &[locked.id],
    )
    .await;
    call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        json!({"issue":locked.number,"head":"locked","base":"main"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(
        db::issues::get_issue_by_id(f.pool(), locked.id)
            .await
            .unwrap()
            .unwrap()
            .locked
    );
    assert!(
        db::pull_requests::records::get_pull_request(f.pool(), repo.id, locked.number)
            .await
            .unwrap()
            .is_none()
    );
    let closed = db::issues::create_issue(
        f.pool(),
        repo.id,
        bob_id,
        "Closed",
        "Opening a pull request reopens the discussion",
    )
    .await
    .unwrap();
    db::issues::update_issue(
        f.pool(),
        closed.id,
        db::discussions::DiscussionEdit {
            actor_id: alice_id,
            expected_state: &closed.state,
            expected_updated: closed.updated_at,
            title: None,
            body: None,
            state: Some("closed"),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let closed = call(
        &router,
        "pulls/create",
        "POST",
        collection,
        owner,
        json!({"issue":closed.number,"head":"closed","base":"main"}),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(closed["state"], "open");
    assert_eq!(f.scalar("SELECT COUNT(*) FROM timeline_events WHERE subject_kind='pull_request' AND subject_id=$1 AND kind='reopened'", &[closed["id"].as_i64().unwrap()]).await, 1);
    // A private fork is published by the real converter, not by borrowing the
    // historical author's identity. Future source changes use that same grant.
    let carol_id = f.actor("carol").await;
    assert!(matches!(
        db::repositories::access::change_personal_repository_collaborator(
            f.pool(),
            alice_id,
            repo.id,
            "bob",
            Some(db::models::AccessLevel::Write)
        )
        .await
        .unwrap(),
        db::repositories::access::CollaboratorChange::Applied(_)
    ));
    let fork = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(bob_id),
        "private-fork",
        None,
        true,
        Some(&repo),
    )
    .await
    .unwrap();
    let written = bob.expect("POST", "/api/repos/bob/private-fork/contents/private.txt", json!({"content":base64::engine::general_purpose::STANDARD.encode("Scoped contribution"),"message":"Private contribution","branch":"main","expected_revision":base}), StatusCode::CREATED).await;
    let private_source = db::issues::create_issue(
        f.pool(),
        repo.id,
        carol_id,
        "Private head",
        "Keep the historical author",
    )
    .await
    .unwrap();
    let private_pull = call(&router, "pulls/create", "POST", collection, reader, json!({"issue":private_source.number,"head":"bob:main","head_repo":"private-fork","base":"main"}), StatusCode::CREATED).await;
    assert_eq!(private_pull["user"]["login"], "carol");
    let native =
        db::pull_requests::records::get_pull_request(f.pool(), repo.id, private_source.number)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(native.head_publisher_id, Some(bob_id));
    assert_eq!(native.maintainer_edit_granted_by_id, Some(bob_id));
    assert_eq!(native.head_repo_id, Some(fork.id));
    let next = bob.expect("POST", "/api/repos/bob/private-fork/contents/later.txt", json!({"content":base64::engine::general_purpose::STANDARD.encode("Later contribution"),"message":"Later contribution","branch":"main","expected_revision":written["sha"]}), StatusCode::CREATED).await;
    let updated = db::pull_requests::records::get_pull_request_by_id(f.pool(), native.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.head_sha, next["sha"].as_str().unwrap());
    assert_eq!(updated.author_id, carol_id);
    let private_url = format!("{collection}/{}", private_source.number);
    call(
        &router,
        "pulls/get",
        "GET",
        &private_url,
        owner,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    call(
        &router,
        "pulls/update",
        "PATCH",
        &private_url,
        owner,
        json!({"title":"Must not mutate an opaque full document"}),
        StatusCode::NOT_FOUND,
    )
    .await;
    let unchanged = db::pull_requests::records::get_pull_request_by_id(f.pool(), native.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.title, "Private head");
    let visible = call(
        &router,
        "pulls/get",
        "GET",
        &private_url,
        reader,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(visible["user"]["login"], "carol");
    assert_eq!(visible["head"]["user"]["login"], "bob");
    let public = call(
        &router,
        "pulls/list",
        "GET",
        collection,
        owner,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(public
        .as_array()
        .unwrap()
        .iter()
        .all(|pull| pull["number"] != private_source.number));
    let page = call(
        &router,
        "pulls/list",
        "GET",
        &format!("{collection}?per_page=1"),
        owner,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(page[0]["number"], closed["number"]);
    // The private repository remains opaque, but its owner granted this exact
    // PR's head update capability. General fork reads/writes are not granted.
    alice.save("main", "upstream.txt", &base).await;
    call(
        &router,
        "pulls/update-branch",
        "PUT",
        &format!("{private_url}/update-branch"),
        owner,
        json!({}),
        StatusCode::ACCEPTED,
    )
    .await;
    let delegated = db::pull_requests::records::get_pull_request_by_id(f.pool(), native.id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(delegated.head_sha, next["sha"].as_str().unwrap());
    call(
        &router,
        "pulls/get",
        "GET",
        &private_url,
        owner,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    // Revoking the real contributor's source access freezes that snapshot even
    // when its historical author and the current source writer can still read it.
    for (repository, actor, name, level) in [
        (repo.id, alice_id, "carol", db::models::AccessLevel::Write),
        (fork.id, bob_id, "carol", db::models::AccessLevel::Read),
        (fork.id, bob_id, "alice", db::models::AccessLevel::Read),
    ] {
        assert!(matches!(
            db::repositories::access::change_personal_repository_collaborator(
                f.pool(),
                actor,
                repository,
                name,
                Some(level)
            )
            .await
            .unwrap(),
            db::repositories::access::CollaboratorChange::Applied(_)
        ));
    }
    let carol = Client::login(&router, f.pool(), carol_id).await;
    let credential = tokens::create(
        f.pool(),
        carol_id,
        Some(&carol.session),
        TokenInput {
            name: "publication",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let credential = format!("Bearer {}", credential.value);
    bob.expect(
        "POST",
        "/api/repos/bob/private-fork/branches",
        json!({"name":"separate","sha":next["sha"]}),
        StatusCode::OK,
    )
    .await;
    let source = db::issues::create_issue(
        f.pool(),
        repo.id,
        alice_id,
        "Separate publisher",
        "No historical-author authority borrowing",
    )
    .await
    .unwrap();
    call(&router, "pulls/create", "POST", collection, &credential, json!({"issue":source.number,"head":"bob:separate","head_repo":"private-fork","base":"main"}), StatusCode::CREATED).await;
    assert!(matches!(
        db::repositories::access::change_personal_repository_collaborator(
            f.pool(),
            bob_id,
            fork.id,
            "carol",
            None
        )
        .await
        .unwrap(),
        db::repositories::access::CollaboratorChange::Applied(_)
    ));
    let refusal = bob
        .expect(
            "POST",
            "/api/repos/bob/private-fork/contents/not-published.txt",
            json!({
                "content":base64::engine::general_purpose::STANDARD.encode("Not an authorized new publication"),
                "message":"After publication revocation","branch":"separate","expected_revision":next["sha"]
            }),
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    assert_eq!(refusal["error"], "file_write_rejected");
    let frozen = db::pull_requests::records::get_pull_request(f.pool(), repo.id, source.number)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frozen.head_publisher_id, Some(carol_id));
    assert_eq!(frozen.author_id, alice_id);
    assert_eq!(frozen.head_sha, next["sha"].as_str().unwrap());
    let live_source = codeyard::backend::repositories::store::repo_store(&fork, None)
        .commit_from_primary("refs/heads/separate")
        .await
        .unwrap();
    assert_ne!(live_source.sha, frozen.head_sha);
    let published = codeyard::backend::repositories::store::repo_store(&repo, None)
        .commit_from_primary(&format!("refs/pull/{}/head", source.number))
        .await
        .unwrap();
    assert_eq!(published.sha, next["sha"].as_str().unwrap());
    // The fork's own writer may still update the branch on purpose, and publishes the new head
    // on their own authority, as efe68f63c made it.
    call(
        &router,
        "pulls/update-branch",
        "PUT",
        &format!("{collection}/{}/update-branch", source.number),
        reader,
        json!({}),
        StatusCode::ACCEPTED,
    )
    .await;
    let updated = db::pull_requests::records::get_pull_request(f.pool(), repo.id, source.number)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(updated.head_sha, frozen.head_sha);
    let published = codeyard::backend::repositories::store::repo_store(&repo, None)
        .commit_from_primary(&format!("refs/pull/{}/head", source.number))
        .await
        .unwrap();
    assert_eq!(published.sha, updated.head_sha);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_issue_conversion() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_issue_conversion() {
    contract(true).await;
}
