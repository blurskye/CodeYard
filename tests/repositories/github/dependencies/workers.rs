use super::*;
use codeyard_app::repositories::pull_requests::mutations::update_pull_request;
use codeyard_app::repositories::pull_requests::mutations::UpdatePullRequestRequest;
use codeyard_repos::editor::FileWrite;
use codeyard_app::repositories::store::repo_store;
use codeyard_db_repos::repositories::dependencies as records;
use codeyard_db_repos::repositories::dependencies::AlertState;
fn seed(w: &World, provider: &provider::Provider) -> String {
    let git = git2::Repository::open_bare(&w.repo.disk_path).unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    let mut tree = git.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    for (path, bytes) in [
        ("package.json", provider.manifest("1.2.0")),
        ("package-lock.json", provider.lock("1.2.0")),
    ] {
        tree.insert(path, git.blob(&bytes).unwrap(), 0o100644)
            .unwrap();
    }
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "Add dependency inventory",
        &tree,
        &[&parent],
    )
    .unwrap()
    .to_string()
}
fn blob(w: &World, sha: &str, path: &str) -> Value {
    let git = git2::Repository::open_bare(&w.repo.disk_path).unwrap();
    let commit = git.find_commit(git2::Oid::from_str(sha).unwrap()).unwrap();
    let tree = commit.tree().unwrap();
    let entry = tree.get_name(path).unwrap();
    let blob = git.find_blob(entry.id()).unwrap();
    serde_json::from_slice(blob.content()).unwrap()
}
async fn save(w: &World, path: &str, bytes: &[u8]) {
    let store = repo_store(&w.repo, None);
    let sha = store.refs_from_primary().await.unwrap()["refs/heads/main"].clone();
    store
        .save_file_as(
            w.f.pool(),
            w.repo.id,
            w.owner,
            &FileWrite {
                branch: "main".into(),
                path: path.into(),
                message: "Accept reviewed security update".into(),
                expected_revision: sha,
                creating: false,
                deleting: false,
            },
            bytes,
            "Alice",
            "alice@example.test",
        )
        .await
        .unwrap();
}
async fn scan(w: &World) {
    codeyard_app::vulnerabilities::ingest(w.f.pool(), &provider::advisory())
        .await
        .unwrap();
    codeyard_db_security::vulnerabilities::enqueue(w.f.pool(), w.repo.id)
        .await
        .unwrap();
    // Every new repository queues its first scan, so the PostgreSQL fixture's repository may
    // come first; it is claimed and left alone.
    let (id, generation) = loop {
        let (id, generation) = codeyard_db_security::vulnerabilities::claim(w.f.pool())
            .await
            .unwrap()
            .unwrap();
        if id == w.repo.id {
            break (id, generation);
        }
    };
    let repo = w.f.repository(id).await;
    codeyard_app::vulnerabilities::scan(&w.state, &repo, generation)
        .await
        .unwrap();
}
async fn run(w: &World, provider: &provider::Provider) {
    let client = registry::Client::new(&provider.origin).unwrap();
    assert!(worker::run_once(w.f.pool(), &w.ci, &w.broadcaster, &client)
        .await
        .unwrap());
}
async fn proposals(pg: bool) {
    let w = setup(pg).await;
    let provider = provider::Provider::new().await;
    let base = seed(&w, &provider);
    // A private repository's dependency graph, and so its scan, is on once Dependabot alerts are.
    enable(&w, &w.writer).await;
    scan(&w).await;
    run(&w, &provider).await;
    let snapshot = records::snapshot(w.f.pool(), w.owner, w.repo.id)
        .await
        .unwrap();
    assert_eq!(snapshot.packages.len(), 1);
    assert_eq!(snapshot.alerts.len(), 1);
    let (key, alert) = snapshot.alerts.iter().next().unwrap();
    assert_eq!(alert.state, AlertState::Open);
    let update = alert.update.as_ref().unwrap();
    let pull_id = update.pull_request_id.unwrap();
    let pull =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(w.f.pool(), pull_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(pull.head_sha, update.commit_sha);
    assert_eq!(pull.head_branch, update.branch);
    assert_eq!(pull.head_repo_id, Some(w.repo.id));
    assert_eq!(pull.state, "open");
    assert!(pull.body.contains("never auto-merges"));
    let store = repo_store(&w.repo, None);
    let refs = store.refs_from_primary().await.unwrap();
    assert_eq!(refs["refs/heads/main"], base);
    assert_eq!(
        refs[&format!("refs/heads/{}", update.branch)],
        update.commit_sha
    );
    let commit = store.commit_from_primary(&update.commit_sha).await.unwrap();
    assert_eq!(commit.parents, vec![base.clone()]);
    assert_eq!(
        blob(&w, &update.commit_sha, "package.json")["dependencies"]["fixture"],
        "^1.2.1"
    );
    assert_eq!(
        blob(&w, &update.commit_sha, "package-lock.json")["packages"]["node_modules/fixture"]
            ["version"],
        "1.2.1"
    );
    assert!(provider.queries.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    let html = w.client.page_ok("/alice/deps/security/dependencies").await;
    assert!(html.contains(&format!("/alice/deps/pulls/{}", pull.number)));
    assert!(html.contains("fixture 1.2.0"));
    assert!(!html.contains("<script>not markup</script>"));
    let id = w.repo.id.to_string();
    let alerts = codeyard_db_security::dependabot_alerts::alerts(w.f.pool(), w.owner, w.repo.id)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    let review = format!(
        "/alice/deps/security/dependabot/{}/dismiss",
        alerts[0].number
    );
    assert_eq!(
        w.client
            .form(
                &review,
                &[
                    ("repository_id", &id),
                    ("state", "dismissed"),
                    ("dismissed_reason", "tolerable_risk"),
                    ("dismissed_comment", "Test-only dependency reviewed"),
                ]
            )
            .await
            .0,
        StatusCode::SEE_OTHER
    );
    records::enqueue(w.f.pool(), w.repo.id).await.unwrap();
    run(&w, &provider).await;
    assert_eq!(
        records::snapshot(w.f.pool(), w.owner, w.repo.id)
            .await
            .unwrap()
            .alerts[key]
            .state,
        AlertState::Dismissed
    );
    checked(
        &w.router,
        "dependabot/update-alert",
        "PATCH",
        &format!("{BASE}/dependabot/alerts/{}", alerts[0].number),
        Some(&w.writer),
        json!({"state":"open"}),
        StatusCode::OK,
    )
    .await;
    records::enqueue(w.f.pool(), w.repo.id).await.unwrap();
    run(&w, &provider).await;
    let actual =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(w.f.pool(), pull_id)
            .await
            .unwrap()
            .unwrap();
    update_pull_request(
        w.f.pool(),
        &w.ci,
        &w.broadcaster,
        w.owner,
        (actual.repo_id, actual.number),
        UpdatePullRequestRequest {
            expected_state: actual.state.clone(),
            expected_updated: actual.updated_at.to_rfc3339(),
            title: None,
            body: None,
            state: Some("closed".into()),
            base: None,
            maintainer_can_modify: None,
        },
    )
    .await
    .unwrap();
    records::enqueue(w.f.pool(), w.repo.id).await.unwrap();
    run(&w, &provider).await;
    let actual =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(w.f.pool(), pull_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(actual.state, "closed");
    assert_eq!(
        records::snapshot(w.f.pool(), w.owner, w.repo.id)
            .await
            .unwrap()
            .alerts[key]
            .update
            .as_ref()
            .unwrap()
            .pull_request_id,
        Some(pull_id)
    );
    let pulls = w
        .client
        .expect(
            "GET",
            "/api/repos/alice/deps/pulls?state=all",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(pulls.as_array().unwrap().len(), 1);
    // Missing lock entries cannot falsely resolve a still-declared vulnerability.
    let incomplete = serde_json::to_vec(
        &json!({"lockfileVersion":3,"packages":{"":{"dependencies":{"fixture":"^1.2.0"}}}}),
    )
    .unwrap();
    save(&w, "package-lock.json", &incomplete).await;
    run(&w, &provider).await;
    let partial = records::snapshot(w.f.pool(), w.owner, w.repo.id)
        .await
        .unwrap();
    // The updater cannot replace the shared scanner's last good inventory with a partial one.
    // The scanner has no inventory of this commit, so the job stops and is retried.
    assert_eq!(partial.packages[0].version, "1.2.0");
    assert_eq!(
        records::state(w.f.pool(), w.repo.id)
            .await
            .unwrap()
            .unwrap()
            .last_status,
        "error"
    );
    assert_eq!(partial.alerts[key].state, AlertState::Open);
    // Actual default-branch file publication—not a hand-set flag—queues a fresh scan.
    save(&w, "package.json", &provider.manifest("1.2.1")).await;
    save(&w, "package-lock.json", &provider.lock("1.2.1")).await;
    scan(&w).await;
    run(&w, &provider).await;
    let snapshot = records::snapshot(w.f.pool(), w.owner, w.repo.id)
        .await
        .unwrap();
    assert_eq!(snapshot.packages[0].version, "1.2.1");
    assert_eq!(snapshot.alerts[key].state, AlertState::Resolved);
}
async fn bad_archive(pg: bool) {
    let w = setup(pg).await;
    let provider = provider::Provider::new().await;
    let base = seed(&w, &provider);
    provider
        .tamper
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // A private repository's dependency graph, and so its scan, is on once Dependabot alerts are.
    enable(&w, &w.writer).await;
    scan(&w).await;
    run(&w, &provider).await;
    let snapshot = records::snapshot(w.f.pool(), w.owner, w.repo.id)
        .await
        .unwrap();
    let alert = snapshot.alerts.values().next().unwrap();
    assert!(alert.update.is_none());
    assert!(alert
        .manual_reason
        .as_ref()
        .unwrap()
        .contains("could not be verified"));
    assert_eq!(
        repo_store(&w.repo, None).refs_from_primary().await.unwrap()["refs/heads/main"],
        base
    );
    let pulls = w
        .client
        .expect(
            "GET",
            "/api/repos/alice/deps/pulls",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert!(pulls.as_array().unwrap().is_empty());
}
async fn revoked(pg: bool) {
    let w = setup(pg).await;
    let provider = provider::Provider::new().await;
    seed(&w, &provider);
    let bob = w.f.actor("bob").await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        w.f.pool(),
        bob,
        w.repo.id,
        codeyard_db_core::models::AccessLevel::Admin,
    )
    .await
    .unwrap();
    let token = credential(&w.router, &w.f, bob, "write").await;
    enable(&w, &token).await;
    checked(
        &w.router,
        "repos/remove-collaborator",
        "DELETE",
        &format!("{BASE}/collaborators/bob"),
        Some(&w.writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    let client = registry::Client::new(&provider.origin).unwrap();
    worker::run_once(w.f.pool(), &w.ci, &w.broadcaster, &client)
        .await
        .unwrap();
    assert_eq!(
        provider.queries.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(fixes(&w).await, json!({"enabled":true,"paused":true}));
}
async fn default_collision(pg: bool) {
    use sha2::{Digest, Sha256};
    let w = setup(pg).await;
    let provider = provider::Provider::new().await;
    let base = seed(&w, &provider);
    let key = records::Finding {
        package: records::InstalledPackage {
            path: "node_modules/fixture".into(),
            name: "fixture".into(),
            version: "1.2.0".into(),
        },
        advisory_id: "GHSA-2345-6789-cfgh".into(),
        summary: String::new(),
        details: String::new(),
        severity: "UNKNOWN".into(),
        url: "https://osv.dev/vulnerability/GHSA-2345-6789-cfgh".into(),
        fixed_versions: vec!["1.2.1".into()],
    }
    .key();
    // The proposal branch is named by the update it carries: path, package, both versions and
    // the scanned commit.
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&("node_modules/fixture", "fixture", "1.2.0", "1.2.1", &base)).unwrap(),
    ));
    let branch = format!("codeyard/security/{digest}");
    repo_store(&w.repo, None)
        .create_branch_as(w.f.pool(), w.repo.id, w.owner, &branch, &base)
        .await
        .unwrap();
    checked(
        &w.router,
        "repos/update",
        "PATCH",
        BASE,
        Some(&w.writer),
        json!({"default_branch":branch.clone()}),
        StatusCode::OK,
    )
    .await;
    // A private repository's dependency graph, and so its scan, is on once Dependabot alerts are.
    enable(&w, &w.writer).await;
    scan(&w).await;
    run(&w, &provider).await;
    let snapshot = records::snapshot(w.f.pool(), w.owner, w.repo.id)
        .await
        .unwrap();
    let alert = snapshot.alerts.get(&key).unwrap();
    assert!(alert.update.is_none());
    assert!(alert
        .manual_reason
        .as_ref()
        .unwrap()
        .contains("is the default branch"));
    assert_eq!(
        repo_store(&w.repo, None).refs_from_primary().await.unwrap()
            [&format!("refs/heads/{branch}")],
        base
    );
    let pulls = w
        .client
        .expect(
            "GET",
            "/api/repos/alice/deps/pulls",
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert!(pulls.as_array().unwrap().is_empty());
}
#[tokio::test]
async fn dependency_proposals_never_publish_the_default_even_on_name_collision() {
    default_collision(false).await;
    default_collision(true).await;
}
#[tokio::test]
async fn real_dependency_scan_and_reviewed_update_on_both_engines() {
    proposals(false).await;
    proposals(true).await;
}
#[tokio::test]
async fn dependency_archive_identity_failure_stays_manual_on_both_engines() {
    bad_archive(false).await;
    bad_archive(true).await;
}
#[tokio::test]
async fn revoked_dependency_delegation_never_exports_on_both_engines() {
    revoked(false).await;
    revoked(true).await;
}
