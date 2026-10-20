use crate::support::{database::Database, fake_webui_bundle as bundle, pull_requests::Client};
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    repositories::{access, languages, lifecycle, store::repo_store},
    storage::ObjectStore,
};
use serde_json::{json, Value};
use std::sync::Arc;

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
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
    let outsider = Client::login(&router, f.pool(), bob).await;
    let rid = repository.id.to_string();
    let tip = client.save("main", "lib.rs", "unborn").await;
    let api = "/api/repos/alice/proj";

    // Invalid supplied access never replaces an existing writer grant.
    client
        .expect(
            "POST",
            &format!("{api}/collaborators"),
            json!({"username":"bob", "access_level":"write"}),
            StatusCode::OK,
        )
        .await;
    for level in ["wirte", "maintain", ""] {
        client
            .expect(
                "POST",
                &format!("{api}/collaborators"),
                json!({"username":"bob", "access_level":level}),
                StatusCode::BAD_REQUEST,
            )
            .await;
        assert_eq!(
            db::repositories::access::get_user_repo_access(f.pool(), bob, repository.id)
                .await
                .unwrap(),
            db::models::AccessLevel::Write
        );
    }

    let label = client
        .expect(
            "POST",
            &format!("{api}/labels"),
            json!({"name":"bug", "color":"123456"}),
            StatusCode::CREATED,
        )
        .await;
    client
        .expect(
            "POST",
            &format!("{api}/labels"),
            json!({"name":"bug", "color":"123456"}),
            StatusCode::CONFLICT,
        )
        .await;
    let other = db::labels::create_label(f.pool(), alice, repository.id, "other", "#123456", None)
        .await
        .unwrap();
    let duplicate = db::labels::update_label(
        f.pool(),
        alice,
        repository.id,
        other.id,
        Some("bug"),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        duplicate.downcast_ref::<db::labels::LabelWriteRefusal>(),
        Some(&db::labels::LabelWriteRefusal::DuplicateName)
    );
    for (action, extra) in [
        ("label", Vec::new()),
        ("edit-label", vec![("label_id", other.id.to_string())]),
    ] {
        let mut pairs = vec![
            ("repository_id", rid.as_str()),
            ("name", "bug"),
            ("color", "#123456"),
        ];
        for (key, value) in &extra {
            pairs.push((key, value));
        }
        let reply = client
            .form_reply(&format!("/alice/proj/_forms/{action}"), &pairs)
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text());
        assert!(reply.text().contains("Choose another name"));
    }
    let (_, html) = client.page("/alice/proj/labels").await;
    for action in ["label", "edit-label", "delete-label"] {
        for form in html.split("<form").filter(|form| {
            form.split('>')
                .next()
                .unwrap()
                .contains(&format!("/_forms/{action}\""))
        }) {
            let form = form.split("</form>").next().unwrap();
            assert!(form.contains("name=\"repository_id\""), "{form}");
            assert!(form.contains(&format!("value=\"{rid}\"")), "{form}");
        }
    }

    // Both allowance lists share resolution, without changing first-occurrence ordering.
    let rule = client.expect("PUT", &format!("{api}/branch-protection"), json!({
        "pattern":"main", "require_pull_request":false,
        "push_allowances":["bob","alice","bob"], "force_push_allowances":["alice","bob","alice"]
    }), StatusCode::OK).await;
    assert_eq!(rule["push_allowances"], json!(["bob", "alice"]));
    assert_eq!(rule["force_push_allowances"], json!(["alice", "bob"]));
    for checks in [vec!["x".repeat(513)], vec!["ok".to_owned(); 101]] {
        client.expect("PUT", &format!("{api}/branch-protection"), json!({"pattern":"too-many", "require_status_checks":true, "required_status_checks":checks}), StatusCode::BAD_REQUEST).await;
    }
    for index in 1..100 {
        db::repositories::protection::upsert_branch_protection_rule(
            f.pool(),
            db::repositories::protection::BranchProtectionRuleUpsert {
                actor_id: alice,
                repo_id: repository.id,
                pattern: &format!("rule-{index}"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    client
        .expect(
            "PUT",
            &format!("{api}/branch-protection"),
            json!({"pattern":"overflow"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    client
        .expect(
            "PUT",
            &format!("{api}/branch-protection"),
            json!({"pattern":"main", "require_pull_request":false}),
            StatusCode::OK,
        )
        .await;

    // Summaries choose the oldest per head, while comparison filters the exact pair.
    for (number, base) in [(1, "main"), (2, "topic")] {
        f.execute(&format!("INSERT INTO pull_requests(repo_id,number,author_id,title,body,head_repo_id,head_branch,base_branch,head_sha,base_sha,state) VALUES($1,{number},$2,'pull-{number}','{}',$1,'feature','{base}','{tip}','{tip}','open')", "b".repeat(65536)), &[repository.id, alice]).await;
    }
    let summaries = db::pull_requests::records::list_open_pull_requests_from_branches(
        f.pool(),
        repository.id,
        None,
    )
    .await
    .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].number, 1);
    let pair = db::pull_requests::records::find_open_pull_request_from_branches(
        f.pool(),
        repository.id,
        "topic",
        "feature",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(pair.number, 2);
    assert!(
        db::pull_requests::records::find_open_pull_request_from_branches(
            f.pool(),
            repository.id,
            "absent",
            "feature"
        )
        .await
        .unwrap()
        .is_none()
    );

    // An archived private repository remains indistinguishable from an absent repository.
    db::repositories::access::revoke_repo_access(f.pool(), bob, repository.id)
        .await
        .unwrap();
    f.execute(
        "UPDATE repositories SET is_private=TRUE,is_archived=TRUE WHERE id=$1",
        &[repository.id],
    )
    .await;
    assert_eq!(
        access::check_write(f.pool(), repository.id, bob)
            .await
            .unwrap_err(),
        access::AccessError::NotFound
    );
    outsider
        .expect(
            "DELETE",
            &format!("{api}/branches/probe"),
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    f.execute(
        "UPDATE repositories SET is_private=FALSE,is_archived=FALSE WHERE id=$1",
        &[repository.id],
    )
    .await;

    // Metadata mutations and non-Git settings sections work even beyond the Git read limit.
    let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let oid = git2::Oid::from_str(&tip).unwrap();
    for index in 0..10_257 {
        bare.reference(&format!("refs/heads/branch-{index}"), oid, false, "fixture")
            .unwrap();
    }
    client.expect("GET", api, Value::Null, StatusCode::OK).await;
    for tab in ["branches", "collaborators", "webhooks", "deploy-keys"] {
        let (status, html) = client
            .page(&format!("/alice/proj/settings?tab={tab}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{html}");
    }
    let reply = client
        .form_reply(
            "/alice/proj/_forms/label",
            &[
                ("repository_id", &rid),
                ("name", "offline-label"),
                ("color", "123456"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let renamed = client
        .expect(
            "POST",
            &format!("{api}/rename"),
            json!({"new_name":"renamed"}),
            StatusCode::OK,
        )
        .await;
    assert_eq!(renamed["name"], "renamed");
    drop(bare);

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
    for action in ["label", "edit-label", "delete-label"] {
        let reply = client
            .form_reply(
                &format!("/alice/proj/_forms/{action}"),
                &[
                    ("repository_id", &rid),
                    ("name", "bug"),
                    ("color", "123456"),
                    ("label_id", &label["id"].to_string()),
                    ("confirm", "yes"),
                ],
            )
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text());
    }
    assert!(db::labels::list_repo_labels(f.pool(), replacement.id)
        .await
        .unwrap()
        .is_empty());

    // A missing Git directory is not a prerequisite for lifecycle deletion.
    std::fs::remove_dir_all(&replacement.disk_path).unwrap();
    let reply = client
        .form_reply(
            "/alice/proj/_forms/delete",
            &[
                ("repository_id", &replacement.id.to_string()),
                ("confirmation", "alice/proj"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert!(
        db::repositories::records::get_repository_by_id(f.pool(), replacement.id)
            .await
            .unwrap()
            .is_none()
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_repository_audit_regressions() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_repository_audit_regressions() {
    contract(true).await;
}

#[tokio::test]
async fn language_cache_rejects_old_primaries_and_versions_including_empty_results() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("cache_owner").await;
        let root = tempfile::tempdir().unwrap();
        let repository = lifecycle::provision(
            (f.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(owner),
            "cache",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let source = db::repository_stats::language_source(f.pool(), repository.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !db::repository_stats::languages_computed(f.pool(), &source, "empty")
                .await
                .unwrap()
        );
        assert!(
            db::repository_stats::upsert_repo_languages(f.pool(), &source, "empty", &[])
                .await
                .unwrap()
        );
        assert!(
            db::repository_stats::languages_computed(f.pool(), &source, "empty")
                .await
                .unwrap()
        );
        assert!(
            !db::repository_stats::languages_computed(f.pool(), &source, "another")
                .await
                .unwrap()
        );
        let tree_id = {
            let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
            let mut builder = bare.treebuilder(None).unwrap();
            builder
                .insert("image.png", bare.blob(b"image bytes").unwrap(), 0o100644)
                .unwrap();
            let tree_id = builder.write().unwrap();
            let tree = bare.find_tree(tree_id).unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
            bare.commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Assets only",
                &tree,
                &[],
            )
            .unwrap();
            tree_id.to_string()
        };
        let store = repo_store(&repository, None);
        assert!(
            languages::compute_and_store(f.pool(), &store, &repository, "main")
                .await
                .unwrap()
                .is_empty()
        );
        // Removing the tree makes a repeated scan fail, but a cached empty result needs no scan.
        std::fs::remove_file(
            std::path::Path::new(&repository.disk_path)
                .join("objects")
                .join(&tree_id[..2])
                .join(&tree_id[2..]),
        )
        .unwrap();
        assert!(
            languages::compute_and_store(f.pool(), &store, &repository, "main")
                .await
                .unwrap()
                .is_empty()
        );
        f.execute(
            "UPDATE repositories SET write_version=write_version+1 WHERE id=$1",
            &[repository.id],
        )
        .await;
        let mut current = db::repository_stats::language_source(f.pool(), repository.id)
            .await
            .unwrap()
            .unwrap();
        let python = vec![("Python".into(), 100, 100.0, "#123456".into())];
        assert!(
            db::repository_stats::upsert_repo_languages(f.pool(), &current, "python", &python)
                .await
                .unwrap()
        );
        assert!(!db::repository_stats::upsert_repo_languages(
            f.pool(),
            &source,
            "old-version",
            &[]
        )
        .await
        .unwrap());
        current.storage_node_id = Some(999);
        assert!(!db::repository_stats::upsert_repo_languages(
            f.pool(),
            &current,
            "old-primary",
            &[]
        )
        .await
        .unwrap());
        assert_eq!(
            languages::stored(f.pool(), repository.id).await.unwrap()[0].language,
            "Python"
        );

        // Size accounting changes only size, never a concurrently selected default branch.
        db::repositories::records::set_repository_default_branch(
            f.pool(),
            owner,
            repository.id,
            "topic",
        )
        .await
        .unwrap();
        db::repositories::records::update_repo_size(f.pool(), repository.id, 123)
            .await
            .unwrap();
        assert_eq!(f.repository(repository.id).await.default_branch, "topic");
        f.finish().await;
    }
}

#[tokio::test]
async fn completed_fork_survives_accounting_failure() {
    for pg in [false, true] {
        let mut f = Database::new(pg).await;
        let owner = f.actor("fork_owner").await;
        let actor = f.actor("fork_actor").await;
        let root = tempfile::tempdir().unwrap();
        let source = lifecycle::provision(
            (f.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(owner),
            "source",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        if pg {
            f.execute("CREATE FUNCTION refuse_snapshot() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture accounting failure'; END $$", &[]).await;
            f.execute("CREATE TRIGGER refuse_snapshot BEFORE INSERT ON repo_storage_history FOR EACH ROW EXECUTE FUNCTION refuse_snapshot()", &[]).await;
        } else {
            f.execute("CREATE TRIGGER refuse_snapshot BEFORE INSERT ON repo_storage_history BEGIN SELECT RAISE(ABORT,'fixture accounting failure'); END", &[]).await;
        }
        let fork = lifecycle::provision(
            (f.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(actor),
            "fork",
            None,
            false,
            Some(&source),
        )
        .await
        .unwrap();
        assert!(
            db::repositories::records::get_repository_by_id(f.pool(), fork.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(repo_store(&fork, None).info().await.is_ok());
        f.finish().await;
    }
}
