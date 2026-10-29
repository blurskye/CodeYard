use super::github::request;
use crate::support::{
    database::{wait_until_blocked, Database},
    http_client::Client,
};
use axum::http::StatusCode;
use codeyard::backend::{
    db::{
        self,
        models::{AccessLevel, PullRequest},
        pull_caps::PullCapReached,
        pull_requests::records::NewPullRequest,
    },
    identity::tokens::{self, TokenInput},
};
use serde_json::json;

async fn open(
    pool: &db::DbPool,
    repo: i64,
    author: i64,
    branch: &str,
    draft: bool,
) -> anyhow::Result<PullRequest> {
    db::pull_requests::records::create_pull_request(
        pool,
        NewPullRequest {
            repo_id: repo,
            head_repo_id: Some(repo),
            author_id: author,
            issue: None,
            maintainer_can_modify: false,
            title: branch,
            body: "",
            head_branch: branch,
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "0000000000000000000000000000000000000000",
            is_draft: draft,
        },
    )
    .await
}
async fn state(
    pool: &db::DbPool,
    pr: &PullRequest,
    actor: i64,
    state: &str,
) -> anyhow::Result<Option<PullRequest>> {
    db::pull_requests::records::update_pull_request(
        pool,
        pr.id,
        db::discussions::DiscussionEdit {
            actor_id: actor,
            expected_state: &pr.state,
            expected_updated: pr.updated_at,
            title: None,
            body: None,
            state: Some(state),
        },
        db::pull_requests::records::PullRequestUpdate {
            base: None,
            maintainer_can_modify: None,
        },
    )
    .await
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let dave = f.actor("dave").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let bob_session = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (actor, session, scope) in [
        (alice, &session.session, "write"),
        (bob, &bob_session.session, "write"),
        (alice, &session.session, "read"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let write = Some(credentials[0].as_str());
    session
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"project"}),
            StatusCode::CREATED,
        )
        .await;
    let project = db::repositories::records::get_repository(f.pool(), "alice", "project")
        .await
        .unwrap()
        .unwrap();
    let (_, cap) = request(
        &router,
        "interactions/get-pull-request-creation-cap-for-repo",
        "GET",
        "/api/v3/repos/alice/project/interaction-limits/pulls/creation-cap",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(
        cap,
        json!({"enabled":false,"max_open_pull_requests":1,"include_drafts":false})
    );
    request(
        &router,
        "interactions/update-pull-request-creation-cap-for-repo",
        "PATCH",
        "/api/v3/repos/alice/project/interaction-limits/pulls/creation-cap",
        write,
        json!({"enabled":true,"max_open_pull_requests":0}),
        422,
    )
    .await;
    request(
        &router,
        "interactions/update-pull-request-creation-cap-for-repo",
        "PATCH",
        "/api/v3/repos/alice/project/interaction-limits/pulls/creation-cap",
        Some(&credentials[2]),
        json!({"enabled":true}),
        403,
    )
    .await;
    request(
        &router,
        "interactions/update-pull-request-creation-cap-for-repo",
        "PATCH",
        "/api/v3/repos/alice/project/interaction-limits/pulls/creation-cap",
        write,
        json!({"enabled":true}),
        200,
    )
    .await;
    db::repositories::access::grant_repo_access(f.pool(), bob, project.id, AccessLevel::Maintain)
        .await
        .unwrap();
    request(
        &router,
        "interactions/get-pull-request-creation-cap-for-repo",
        "GET",
        "/api/v3/repos/alice/project/interaction-limits/pulls/creation-cap",
        Some(&credentials[1]),
        json!(null),
        403,
    )
    .await;
    request(
        &router,
        "interactions/set-pull-request-bypass-list-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        Some(&credentials[1]),
        json!({"users":["alice","alice"]}),
        204,
    )
    .await;
    // Usernames are exact; `Alice` could be another account, so it is not read as `alice`.
    request(
        &router,
        "interactions/set-pull-request-bypass-list-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        Some(&credentials[1]),
        json!({"users":["Alice"]}),
        422,
    )
    .await;
    let (_, bypass) = request(
        &router,
        "interactions/get-pull-request-bypass-list-for-repo",
        "GET",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        Some(&credentials[1]),
        json!(null),
        200,
    )
    .await;
    assert_eq!(bypass.as_array().unwrap().len(), 1);
    assert_eq!(bypass[0]["login"], "alice");
    request(
        &router,
        "interactions/remove-pull-request-bypass-list-for-repo",
        "DELETE",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":["alice"]}),
        204,
    )
    .await;
    request(
        &router,
        "interactions/set-pull-request-bypass-list-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":["bob","missing-account"]}),
        422,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM interaction_pull_bypass WHERE repo_id=$1",
            &[project.id]
        )
        .await,
        0
    );
    // Exercise the real native PR creation path on actual commits, including a quota refusal.
    let (_, commit) = request(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        "/api/v3/repos/alice/project/contents/base.txt",
        write,
        json!({"message":"Base","content":"YmFzZQo="}),
        201,
    )
    .await;
    let sha = commit["commit"]["sha"].as_str().unwrap();
    request(
        &router,
        "git/create-ref",
        "POST",
        "/api/v3/repos/alice/project/git/refs",
        write,
        json!({"ref":"refs/heads/feature-one","sha":sha}),
        201,
    )
    .await;
    let (_, commit) = request(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        "/api/v3/repos/alice/project/contents/feature.txt",
        write,
        json!({"message":"Feature","content":"ZmVhdHVyZQo=","branch":"feature-one"}),
        201,
    )
    .await;
    request(
        &router,
        "git/create-ref",
        "POST",
        "/api/v3/repos/alice/project/git/refs",
        write,
        json!({"ref":"refs/heads/feature-two","sha":commit["commit"]["sha"]}),
        201,
    )
    .await;
    let first = session
        .expect(
            "POST",
            "/api/repos/alice/project/pulls",
            json!({"title":"First","head_branch":"feature-one","base_branch":"main"}),
            StatusCode::CREATED,
        )
        .await;
    let denied = session
        .expect(
            "POST",
            "/api/repos/alice/project/pulls",
            json!({"title":"Capped","head_branch":"feature-two","base_branch":"main"}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(denied["error"], "pull_request_creation_cap");
    let draft = session.expect("POST","/api/repos/alice/project/pulls",json!({"title":"Draft","head_branch":"feature-two","base_branch":"main","is_draft":true}),StatusCode::CREATED).await;
    let first = db::pull_requests::records::get_pull_request(
        f.pool(),
        project.id,
        first["number"].as_i64().unwrap() as i32,
    )
    .await
    .unwrap()
    .unwrap();
    let draft = db::pull_requests::records::get_pull_request(
        f.pool(),
        project.id,
        draft["number"].as_i64().unwrap() as i32,
    )
    .await
    .unwrap()
    .unwrap();
    let error =
        db::pull_requests::records::mark_pull_request_ready(f.pool(), draft.id, alice, None)
            .await
            .unwrap_err();
    assert!(error.is::<PullCapReached>());
    let closed = state(f.pool(), &first, alice, "closed")
        .await
        .unwrap()
        .unwrap();
    let ready =
        db::pull_requests::records::mark_pull_request_ready(f.pool(), draft.id, alice, None)
            .await
            .unwrap();
    assert!(matches!(
        ready,
        db::pull_requests::records::ReadyOutcome::Ready(_)
    ));
    assert!(state(f.pool(), &closed, alice, "open")
        .await
        .unwrap_err()
        .is::<PullCapReached>());
    // A different author has an independent counter, even when another author filled theirs.
    open(f.pool(), project.id, bob, "bob-first", false)
        .await
        .unwrap();
    assert!(open(f.pool(), project.id, bob, "bob-second", false)
        .await
        .unwrap_err()
        .is::<PullCapReached>());
    // Conversion keeps the issue author: the converter's bypass must not evade that quota.
    let issue = bob_session
        .expect(
            "POST",
            "/api/repos/alice/project/issues",
            json!({"title":"Bob's converted issue"}),
            StatusCode::CREATED,
        )
        .await;
    let number = issue["number"].as_i64().unwrap();
    request(
        &router,
        "interactions/set-pull-request-bypass-list-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":["alice"]}),
        204,
    )
    .await;
    let conversion = || NewPullRequest {
        repo_id: project.id,
        head_repo_id: Some(project.id),
        author_id: alice,
        issue: Some(number as i32),
        maintainer_can_modify: false,
        title: "Converted",
        body: "",
        head_branch: "converted-bob",
        head_sha: "1111111111111111111111111111111111111111",
        base_branch: "main",
        base_sha: "0000000000000000000000000000000000000000",
        is_draft: false,
    };
    assert!(
        db::pull_requests::records::create_pull_request(f.pool(), conversion())
            .await
            .unwrap_err()
            .is::<PullCapReached>()
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1 AND number=$2",
            &[project.id, number]
        )
        .await,
        1
    );
    request(
        &router,
        "interactions/remove-pull-request-bypass-list-for-repo",
        "DELETE",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":["alice"]}),
        204,
    )
    .await;
    let reply = session
        .form_reply(
            "/settings/pull-request-bypass",
            &[("repo_id", &project.id.to_string()), ("username", "bob")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let converted = db::pull_requests::records::create_pull_request(f.pool(), conversion())
        .await
        .unwrap();
    assert_eq!(converted.author_id, bob);
    assert_eq!(i64::from(converted.number), number);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM issues WHERE repo_id=$1 AND number=$2",
            &[project.id, number]
        )
        .await,
        0
    );
    open(f.pool(), project.id, bob, "bob-bypasses", false)
        .await
        .unwrap();
    let reply = session
        .form_reply(
            "/settings/pull-request-bypass",
            &[
                ("repo_id", &project.id.to_string()),
                ("username", "bob"),
                ("remove", "yes"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(open(f.pool(), project.id, bob, "bob-capped-again", false)
        .await
        .unwrap_err()
        .is::<PullCapReached>());
    let page = session.page_ok("/alice/project/settings").await;
    assert!(
        page.contains("/settings/pull-request-cap")
            && page.contains("/settings/pull-request-bypass")
    );
    let reply = session
        .form_reply(
            "/settings/pull-request-cap",
            &[
                ("scope", "repository"),
                ("scope_id", &project.id.to_string()),
                ("enabled", "false"),
                ("max_open_pull_requests", "2"),
                ("include_drafts", "true"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let (_, cap) = request(
        &router,
        "interactions/get-pull-request-creation-cap-for-repo",
        "GET",
        "/api/v3/repos/alice/project/interaction-limits/pulls/creation-cap",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(
        cap,
        json!({"enabled":false,"max_open_pull_requests":2,"include_drafts":true})
    );
    // Both the request size and total bypass-list size are bounded, without partial batch effects.
    let mut names = Vec::new();
    for n in 0..100 {
        let name = format!("bypass{n:03}");
        f.actor(&name).await;
        names.push(name);
    }
    request(
        &router,
        "interactions/set-pull-request-bypass-list-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":names}),
        204,
    )
    .await;
    request(
        &router,
        "interactions/set-pull-request-bypass-list-for-repo",
        "PUT",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":["alice"]}),
        422,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM interaction_pull_bypass WHERE repo_id=$1",
            &[project.id]
        )
        .await,
        100
    );
    request(
        &router,
        "interactions/remove-pull-request-bypass-list-for-repo",
        "DELETE",
        "/api/v3/repos/alice/project/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":vec!["alice";101]}),
        422,
    )
    .await;
    let input = db::organizations::profiles::NewOrganization::new(
        "acme",
        db::organizations::profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let org = match db::organizations::profiles::create_organization(f.pool(), alice, &input)
        .await
        .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("organization refused"),
    };
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'admin')",
        &[org.id, bob],
    )
    .await;
    for (name, private) in [("one", false), ("two", false), ("private", true)] {
        session
            .expect(
                "POST",
                "/api/repos",
                json!({"name":name,"owner_org_id":org.id,"is_private":private}),
                StatusCode::CREATED,
            )
            .await;
    }
    let mut repos = Vec::new();
    for name in ["one", "two", "private"] {
        repos.push(
            db::repositories::records::get_repository(f.pool(), "acme", name)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    request(
        &router,
        "interactions/get-pull-request-creation-cap-for-org",
        "GET",
        "/api/v3/orgs/acme/interaction-limits/pulls/creation-cap",
        Some(&credentials[1]),
        json!(null),
        200,
    )
    .await;
    request(
        &router,
        "interactions/update-pull-request-creation-cap-for-org",
        "PATCH",
        "/api/v3/orgs/acme/interaction-limits/pulls/creation-cap",
        Some(&credentials[1]),
        json!({"enabled":true,"max_open_pull_requests":1,"include_drafts":true}),
        200,
    )
    .await;
    open(f.pool(), repos[0].id, alice, "org-first", false)
        .await
        .unwrap();
    assert!(open(f.pool(), repos[1].id, alice, "org-capped", false)
        .await
        .unwrap_err()
        .is::<PullCapReached>());
    open(f.pool(), repos[2].id, alice, "private-unaffected", false)
        .await
        .unwrap();
    request(
        &router,
        "interactions/update-pull-request-creation-cap-for-repo",
        "PATCH",
        "/api/v3/repos/acme/two/interaction-limits/pulls/creation-cap",
        write,
        json!({"enabled":true,"max_open_pull_requests":3,"include_drafts":true}),
        200,
    )
    .await;
    assert!(open(f.pool(), repos[1].id, alice, "both-caps", false)
        .await
        .unwrap_err()
        .is::<PullCapReached>());
    request(
        &router,
        "interactions/set-pull-request-bypass-list-for-repo",
        "PUT",
        "/api/v3/repos/acme/two/interaction-limits/pulls/bypass-list",
        write,
        json!({"users":["alice"]}),
        204,
    )
    .await;
    open(f.pool(), repos[1].id, alice, "both-bypassed", false)
        .await
        .unwrap();
    let (one, two) = tokio::join!(
        open(f.pool(), repos[0].id, carol, "parallel-one", false),
        open(f.pool(), repos[1].id, carol, "parallel-two", false)
    );
    assert_ne!(
        one.is_ok(),
        two.is_ok(),
        "The shared organization counter must permit exactly one creation"
    );
    let page = bob_session
        .page_ok(&format!("/settings/organizations/{}", org.id))
        .await;
    assert!(page.contains("/settings/pull-request-cap"));
    assert!(!page.contains("action=\"/settings/interaction-limits\""));
    if pg {
        request(
            &router,
            "interactions/update-pull-request-creation-cap-for-org",
            "PATCH",
            "/api/v3/orgs/acme/interaction-limits/pulls/creation-cap",
            write,
            json!({"enabled":false}),
            200,
        )
        .await;
        open(f.pool(), repos[0].id, dave, "before-policy", false)
            .await
            .unwrap();
        let mut gate = f.pg().begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),37002)")
            .execute(&mut *gate)
            .await
            .unwrap();
        sqlx::query("UPDATE interaction_pull_caps SET enabled=TRUE WHERE org_id=$1")
            .bind(org.id)
            .execute(&mut *gate)
            .await
            .unwrap();
        let pool = f.pool().clone();
        let repo = repos[1].id;
        let writer = tokio::spawn(async move {
            open(&pool, repo, dave, "after-policy", false)
                .await
                .unwrap_err()
                .is::<PullCapReached>()
        });
        wait_until_blocked(f.pg(), pid, 1).await;
        gate.commit().await.unwrap();
        assert!(writer.await.unwrap());
    }
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_pull_caps() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_pull_caps() {
    contract(true).await;
}
