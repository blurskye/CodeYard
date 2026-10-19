//! Repository form identity, bounded lists and effect-time authority on both databases.
use crate::support::{database::Database, fake_webui_bundle as bundle, pull_requests::Client};
use axum::http::StatusCode;
use codeyard::{
    backend::{
        api::create_router,
        app::AppState,
        ci::CiConfig,
        db,
        repositories::{access, lifecycle, store::repo_store},
        storage::ObjectStore,
    },
    webui::{app::Page, pages::repos as view},
};
use serde_json::{json, Value};
use std::sync::Arc;

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let upper = f.actor("Alice").await;
    let root = tempfile::tempdir().unwrap();
    let repository = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
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
        CiConfig::default(),
        None,
    );
    let router = create_router(state.clone());
    let client = Client::login(&router, f.pool(), alice).await;
    let tip = client.save("main", "lib.rs", "unborn").await;
    let rid = repository.id.to_string();

    // Bounded read batches preserve the same public, owner and grant decisions.
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[repository.id],
    )
    .await;
    assert!(db::repositories::access::readable_repository_ids(
        f.pool(),
        Some(bob),
        &[repository.id]
    )
    .await
    .unwrap()
    .is_empty());
    assert_eq!(
        db::repositories::access::readable_repository_ids(f.pool(), Some(alice), &[repository.id])
            .await
            .unwrap(),
        [repository.id]
    );
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repository.id,
        db::models::AccessLevel::Admin,
    )
    .await
    .unwrap();
    assert_eq!(
        db::repositories::access::readable_repository_ids(f.pool(), Some(bob), &[repository.id])
            .await
            .unwrap(),
        [repository.id]
    );
    access::check_admin(f.pool(), repository.id, bob)
        .await
        .unwrap();
    db::repositories::access::revoke_repo_access(f.pool(), bob, repository.id)
        .await
        .unwrap();
    let refused =
        db::repositories::records::set_repository_archived(f.pool(), bob, repository.id, true)
            .await
            .unwrap_err();
    assert!(matches!(
        refused.downcast_ref::<db::repositories::records::RepositorySettingsRefusal>(),
        Some(db::repositories::records::RepositorySettingsRefusal::Denied)
    ));
    assert!(db::repositories::records::set_repository_archived(
        f.pool(),
        alice,
        repository.id,
        true
    )
    .await
    .unwrap());
    assert!(db::repositories::records::set_repository_archived(
        f.pool(),
        bob,
        repository.id,
        false
    )
    .await
    .is_err());
    assert!(db::repositories::records::set_repository_archived(
        f.pool(),
        alice,
        repository.id,
        false
    )
    .await
    .unwrap());
    f.execute(
        "UPDATE repositories SET is_private=FALSE WHERE id=$1",
        &[repository.id],
    )
    .await;

    // The picker and the submitted allowlist distinguish case-sensitive accounts.
    db::repositories::access::grant_repo_access(
        f.pool(),
        upper,
        repository.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    client.expect("PUT", "/api/repos/alice/proj/branch-protection", json!({"pattern":"main", "require_pull_request":false, "force_push_allowances":["Alice"]}), StatusCode::OK).await;
    let (status, html) = client.page("/alice/proj/settings?tab=branches").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    for (name, checked) in [("alice", false), ("Alice", true)] {
        let input = html
            .split("<input")
            .map(|part| part.split('>').next().unwrap())
            .find(|part| {
                part.contains("name=\"force_push_allowances\"")
                    && part.contains(&format!("value=\"{name}\""))
            })
            .unwrap();
        assert_eq!(input.contains(" checked"), checked, "{input}");
    }
    let reply = client
        .form_reply(
            "/alice/proj/_forms/protection-save",
            &[
                ("repository_id", &rid),
                ("pattern", "main"),
                ("required_approving_reviews", "0"),
                ("force_push_allowances", "alice"),
                ("force_push_allowances", "Alice"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let rules = db::repositories::protection::list_branch_protection_rules(f.pool(), repository.id)
        .await
        .unwrap();
    assert!(rules[0].force_push_allowances.contains(&alice));
    assert!(rules[0].force_push_allowances.contains(&upper));
    let (_, page) = client.model("/alice/proj/settings?tab=general").await;
    let Page::Repositories(view::page::Page::Settings { settings, .. }) = page else {
        panic!("settings page")
    };
    assert!(
        settings.collaborators.is_empty()
            && settings.branch_rules.is_empty()
            && settings.force_push_suggestions.is_empty()
    );
    let (_, page) = client.model("/alice/proj/settings?tab=collaborators").await;
    let Page::Repositories(view::page::Page::Settings { settings, .. }) = page else {
        panic!("settings page")
    };
    assert_eq!(settings.collaborators.len(), 1);
    assert_eq!(settings.collaborators[0].username, "Alice");

    // Status payload pages are bounded, while combined state/count cover every context.
    f.execute(&format!("WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<1023) INSERT INTO commit_statuses(repo_id,sha,context,state,description,creator_id) SELECT $1,'{tip}',CASE WHEN i=1023 THEN 'zz-failed' ELSE 'ctx-' || CAST(i AS TEXT) END,CASE WHEN i=1023 THEN 'failure' ELSE 'success' END,'small',$2 FROM n"), &[repository.id, alice]).await;
    let statuses = format!("/api/repos/alice/proj/commits/{tip}/statuses");
    let page = client
        .expect("GET", &statuses, Value::Null, StatusCode::OK)
        .await;
    assert_eq!(page.as_array().unwrap().len(), 30);
    assert!(page
        .as_array()
        .unwrap()
        .iter()
        .all(|status| status["creator"]["username"] == "alice"));
    let second = client
        .expect(
            "GET",
            &format!("{statuses}?page=2&per_page=100"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(second.as_array().unwrap().len(), 100);
    let combined = client
        .expect(
            "GET",
            &format!("/api/repos/alice/proj/commits/{tip}/status"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(combined["total_count"], 1024);
    assert_eq!(combined["state"], "failure");
    assert_eq!(combined["statuses"].as_array().unwrap().len(), 30);
    client
        .expect(
            "POST",
            &statuses,
            json!({"context":"one-too-many","state":"success"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    client
        .expect(
            "POST",
            &statuses,
            json!({"context":"ctx-0","state":"pending"}),
            StatusCode::CREATED,
        )
        .await;
    client
        .expect(
            "POST",
            &statuses,
            json!({"context":"ctx-0","state":"success","description":"x".repeat(1025)}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    client
        .expect(
            "POST",
            &statuses,
            json!({"context":"ctx-0","state":"success","target_url":"x".repeat(2049)}),
            StatusCode::BAD_REQUEST,
        )
        .await;

    // Search and pagination happen before label models, including legacy long descriptions.
    f.execute("WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<100) INSERT INTO labels(repo_id,name,color,description) SELECT $1,'label-' || CAST(i AS TEXT),'#123456','description' FROM n", &[repository.id]).await;
    let labels = db::labels::list_repo_labels_page(f.pool(), repository.id, "", "name", 101, 0)
        .await
        .unwrap();
    assert_eq!(labels.len(), 101);
    f.execute(
        &format!(
            "UPDATE labels SET description='{}' WHERE id=$1",
            "x".repeat(20_000)
        ),
        &[labels[0].id],
    )
    .await;
    let (_, page) = client.model("/alice/proj/labels").await;
    let model = serde_json::to_string(&page).unwrap();
    assert!(model.len() < 200_000);
    assert!(model.contains("\"has_next\":true"));
    let (_, html) = client.page("/alice/proj/labels?page=2").await;
    assert!(html.contains("Previous"), "{html}");
    let found =
        db::labels::list_repo_labels_page(f.pool(), repository.id, "label-100", "name", 100, 0)
            .await
            .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "label-100");
    client
        .expect(
            "POST",
            "/api/repos/alice/proj/labels",
            json!({"name":"oversized","color":"123456","description":"x".repeat(1025)}),
            StatusCode::BAD_REQUEST,
        )
        .await;

    // The older open PR is not hidden behind 100 newer closed/merged PRs.
    let milestone =
        db::milestones::create_milestone(f.pool(), alice, repository.id, "release", None, None)
            .await
            .unwrap();
    f.execute(&format!("WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<101) INSERT INTO pull_requests(repo_id,number,author_id,title,head_repo_id,head_branch,base_branch,head_sha,base_sha,state,milestone_id) SELECT $1,i+1,$2,CASE WHEN i=0 THEN 'older-open-pull' ELSE 'closed-pull' END,$1,'head-' || CAST(i AS TEXT),'main','{tip}','{tip}',CASE WHEN i=0 THEN 'open' WHEN i=1 THEN 'merged' ELSE 'closed' END,$3 FROM n"), &[repository.id, alice, milestone.id]).await;
    let (status, html) = client.page("/alice/proj/milestones/1?state=open").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("older-open-pull"), "{html}");
    let closed = db::pull_requests::records::list_repo_pull_requests(
        f.pool(),
        repository.id,
        &db::discussions::ListFilter {
            state: Some("closed"),
            include_merged: true,
            milestone: db::discussions::Pick::Is(milestone.id),
            ..Default::default()
        },
        200,
    )
    .await
    .unwrap();
    assert_eq!(closed.len(), 101);
    assert!(closed
        .iter()
        .any(|entry| entry.pull_request.state == "merged"));

    // Forks immediately report their copied storage and populate stored languages.
    let fork = lifecycle::fork(
        f.pool(),
        None,
        root.path(),
        &state.notification_broadcaster,
        bob,
        repository.id,
        "alice",
    )
    .await
    .unwrap();
    assert!(fork.size_bytes > 0);
    assert_eq!(f.repository(fork.id).await.size_bytes, fork.size_bytes);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if !db::repository_stats::get_repo_languages(f.pool(), fork.id)
            .await
            .unwrap()
            .is_empty()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fork languages were not stored"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Git outages keep submitted editor text, including native Preview; stars need only SQL.
    let offline = root.path().join("offline.git");
    std::fs::rename(&repository.disk_path, &offline).unwrap();
    let preview = client
        .form_reply(
            "/alice/proj/_forms/write-file",
            &[
                ("repository_id", &rid),
                ("branch", "main"),
                ("path", "lib.rs"),
                ("content", "kept offline draft"),
                ("expected_revision", &tip),
                ("intent", "preview"),
                ("commit_target", "new_branch"),
                ("new_branch", "offline-draft"),
            ],
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.text());
    assert!(preview.text().contains("kept offline draft"));
    let save = client
        .form_reply(
            "/alice/proj/_forms/write-file",
            &[
                ("repository_id", &rid),
                ("branch", "main"),
                ("path", "lib.rs"),
                ("content", "kept refused draft"),
                ("expected_revision", &tip),
                ("intent", "save"),
                ("message", "offline save"),
            ],
        )
        .await;
    assert!(save.status.is_server_error() || save.status.is_client_error());
    assert!(
        save.text().contains("kept refused draft"),
        "{}",
        save.text()
    );
    let star = client.form_reply("/alice/proj/_forms/star", &[]).await;
    assert_eq!(star.status, StatusCode::SEE_OTHER, "{}", star.text());
    std::fs::rename(&offline, &repository.disk_path).unwrap();

    // Ref forms bind to the original repository, not a replacement at the same URL.
    db::repositories::records::rename_repository(f.pool(), alice, repository.id, "renamed")
        .await
        .unwrap();
    let replacement = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let new_tip = client.save("main", "README.md", "unborn").await;
    let git = git2::Repository::open_bare(&replacement.disk_path).unwrap();
    for reference in ["refs/heads/topic", "refs/tags/v1"] {
        git.reference(
            reference,
            git2::Oid::from_str(&new_tip).unwrap(),
            true,
            "fixture",
        )
        .unwrap();
    }
    for (action, name) in [("delete-branch", "topic"), ("delete-tag", "v1")] {
        let refused = client
            .form_reply(
                &format!("/alice/proj/_forms/{action}"),
                &[("repository_id", &rid), ("name", name), ("confirm", "yes")],
            )
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.text());
    }
    assert!(git.find_reference("refs/heads/topic").is_ok());
    assert!(git.find_reference("refs/tags/v1").is_ok());
    assert_eq!(
        repo_store(&replacement, None).refs().await.unwrap().len(),
        3
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_repository_regressions() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_repository_regressions() {
    contract(true).await;
}
