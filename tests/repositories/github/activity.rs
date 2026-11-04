use super::*;
use crate::support::git_server::{self, Server};

fn linked(headers: &HeaderMap, relation: &str) -> String {
    let link = headers["link"]
        .to_str()
        .unwrap()
        .split(", ")
        .find(|part| part.ends_with(&format!("rel=\"{relation}\"")))
        .unwrap();
    let absolute = link.split('<').nth(1).unwrap().split('>').next().unwrap();
    let url = url::Url::parse(absolute).unwrap();
    format!("{}?{}", url.path(), url.query().unwrap())
}
async fn activity_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let repos = root.path().join("repos");
    let router = f.router(&repos).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let outsider = credential(&router, &f, carol, "write").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj","auto_init":true,"private":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    let base_sha = git2::Repository::open_bare(&repo.disk_path)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let owner = Client::login(&router, f.pool(), alice).await;
    let contributor = Client::login(&router, f.pool(), bob).await;
    contributor.branch("topic", &base_sha).await;
    owner
        .save_text("topic", "change.txt", "A change\n", &base_sha)
        .await;
    let pull = owner
        .open_pull("Merge actual changes", "topic", "main")
        .await;
    owner.merge_current(pull["number"].as_i64().unwrap()).await;
    contributor
        .expect(
            "DELETE",
            "/api/repos/alice/proj/branches/topic",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    // Exercise the same recorder from a real authenticated smart-HTTP receive hook, not a
    // direct SQL event fixture. Resetting the default branch to its ancestor is a force push.
    let server = Server::start(root.path(), &f.connection_url()).await;
    let push_token = git_server::write_token(f.pool(), alice).await;
    let remote = format!("{}/alice/proj.git", server.base);
    git_server::success(git_server::git(
        root.path(),
        &["clone", &remote, "work"],
        Some(&push_token),
    ));
    let work = root.path().join("work");
    git_server::success(git_server::git(
        &work,
        &["reset", "--hard", &base_sha],
        None,
    ));
    git_server::success(git_server::git(
        &work,
        &["push", "--force", "origin", "HEAD:refs/heads/main"],
        Some(&push_token),
    ));
    let endpoint = "/api/v3/repos/alice/proj/activity";
    for token in [None, Some(outsider.as_str())] {
        checked(
            &router,
            "repos/list-activities",
            "GET",
            endpoint,
            token,
            Value::Null,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    let (_, all) = checked(
        &router,
        "repos/list-activities",
        "GET",
        endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let rows = all.as_array().unwrap();
    for kind in [
        "branch_creation",
        "branch_deletion",
        "push",
        "pr_merge",
        "force_push",
    ] {
        assert!(
            rows.iter().any(|row| row["activity_type"] == kind),
            "Missing {kind}: {rows:?}"
        );
    }
    let force = rows
        .iter()
        .find(|row| row["activity_type"] == "force_push")
        .unwrap();
    assert_eq!(force["after"], base_sha);
    assert_eq!(force["actor"]["login"], "alice");
    assert!(force["node_id"].as_str().unwrap().starts_with("RAC_"));
    let deleted = rows
        .iter()
        .find(|row| row["activity_type"] == "branch_deletion")
        .unwrap();
    assert_eq!(deleted["ref"], "refs/heads/topic");
    assert_eq!(deleted["after"], "0".repeat(40));
    assert_eq!(deleted["actor"]["login"], "bob");
    let (headers, first) = checked(
        &router,
        "repos/list-activities",
        "GET",
        &format!("{endpoint}?direction=asc&per_page=1"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first.as_array().unwrap().len(), 1);
    let first_id = first[0]["id"].as_i64().unwrap();
    let (headers, second) = checked(
        &router,
        "repos/list-activities",
        "GET",
        &linked(&headers, "next"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(second[0]["id"].as_i64().unwrap() > first_id);
    let (_, previous) = checked(
        &router,
        "repos/list-activities",
        "GET",
        &linked(&headers, "prev"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(previous[0]["id"], first_id);
    for (query, kind) in [
        ("activity_type=force_push", "force_push"),
        ("activity_type=pr_merge", "pr_merge"),
        (
            "activity_type=branch_deletion&ref=refs%2Fheads%2Ftopic",
            "branch_deletion",
        ),
    ] {
        let (_, rows) = checked(
            &router,
            "repos/list-activities",
            "GET",
            &format!("{endpoint}?{query}"),
            Some(&reader),
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["activity_type"], kind);
    }
    let (_, rows) = checked(
        &router,
        "repos/list-activities",
        "GET",
        &format!("{endpoint}?actor=BOB&per_page=1"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["actor"]["login"], "bob");
    let (_, rows) = checked(
        &router,
        "repos/list-activities",
        "GET",
        &format!("{endpoint}?actor=carol"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(rows.as_array().unwrap().is_empty());
    for query in [
        "direction=sideways",
        "before=broken",
        "after=broken",
        "before=AAAAAAAAAAE&after=AAAAAAAAAAI",
        "time_period=forever",
        "activity_type=comment",
    ] {
        checked(
            &router,
            "repos/list-activities",
            "GET",
            &format!("{endpoint}?{query}"),
            Some(&reader),
            Value::Null,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    // Time filters apply before limits. This moves the genuine initial event out of the window.
    f.execute(
        "UPDATE repository_ref_activities SET created_at='2000-01-01T00:00:00Z' WHERE id=$1",
        &[first_id],
    )
    .await;
    let (_, recent) = checked(
        &router,
        "repos/list-activities",
        "GET",
        &format!("{endpoint}?time_period=day"),
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(recent
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["id"] != first_id));
    assert_eq!(
        recent.as_array().unwrap().len() + 1,
        all.as_array().unwrap().len()
    );
    let html = owner
        .page_ok("/alice/proj/insights/activity?actor=bob")
        .await;
    assert!(html.contains("Repository activity"));
    assert!(html.contains("refs/heads/topic"));
    assert!(html.contains("branch deletion"));
    let stranger = Client::login(&router, f.pool(), carol).await;
    assert_eq!(
        stranger.page("/alice/proj/insights/activity").await.0,
        StatusCode::NOT_FOUND
    );
    // Identity deletion preserves accepted activity without inventing a replacement actor.
    f.execute("DELETE FROM users WHERE id=$1", &[bob]).await;
    let (_, history) = checked(
        &router,
        "repos/list-activities",
        "GET",
        endpoint,
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(history
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["ref"] == "refs/heads/topic")
        .any(|row| row["actor"].is_null()));
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/proj",
        Some(&writer),
        json!({"private":false}),
        StatusCode::OK,
    )
    .await;
    let (_, public) = checked(
        &router,
        "repos/list-activities",
        "GET",
        endpoint,
        None,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        public.as_array().unwrap().len(),
        history.as_array().unwrap().len()
    );
    drop(server);
}
#[tokio::test]
async fn sqlite_github_repository_activity() {
    activity_contract(false).await;
}
#[tokio::test]
async fn postgres_github_repository_activity() {
    activity_contract(true).await;
}
