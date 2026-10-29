use super::{db, json, protection::call, Client, Router, Value};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use tower::ServiceExt;

#[tokio::test]
async fn sqlite_upstream_merge() {
    upstream_contract(false).await;
}

#[tokio::test]
async fn postgres_upstream_merge() {
    upstream_contract(true).await;
}

async fn upstream_contract(pg: bool) {
    use super::{tokens, Database, TokenInput};
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let fork_owner = Client::login(&router, f.pool(), bob).await;
    owner
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"upstream"}),
            StatusCode::CREATED,
        )
        .await;
    let mut auth = Vec::new();
    for (actor, session, scope) in [
        (alice, &owner, "write"),
        (bob, &fork_owner, "write"),
        (bob, &fork_owner, "read"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: "sync",
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        auth.push(format!("Bearer {}", token.value));
    }
    let (upstream_token, writer, reader) = (auth[0].as_str(), auth[1].as_str(), auth[2].as_str());
    let source = "/api/v3/repos/alice/upstream";
    let target = "/api/v3/repos/bob/upstream";
    let seed = call(
        &router,
        "PUT",
        &format!("{source}/contents/seed"),
        upstream_token,
        json!({"message":"Seed","content":"c2VlZA=="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    let seed_sha = seed["commit"]["sha"].as_str().unwrap();
    fork_owner
        .expect(
            "POST",
            "/api/repos/alice/upstream/forks",
            json!({}),
            StatusCode::CREATED,
        )
        .await;
    let endpoint = format!("{target}/merge-upstream");
    call(
        &router,
        "POST",
        &format!("{source}/merge-upstream"),
        upstream_token,
        json!({"branch":"main"}),
        ("repos/merge-upstream", 422),
    )
    .await;
    call(
        &router,
        "POST",
        &endpoint,
        reader,
        json!({"branch":"main"}),
        ("repos/merge-upstream", 403),
    )
    .await;
    for input in [
        json!({}),
        json!({"branch":"bad..name"}),
        json!({"branch":"missing"}),
    ] {
        call(
            &router,
            "POST",
            &endpoint,
            writer,
            input,
            ("repos/merge-upstream", 422),
        )
        .await;
    }
    let synced = call(
        &router,
        "POST",
        &endpoint,
        writer,
        json!({"branch":"main"}),
        ("repos/merge-upstream", 200),
    )
    .await;
    assert_eq!(synced["merge_type"], "none");
    assert_eq!(synced["base_branch"], "alice:main");
    let advance = call(
        &router,
        "PUT",
        &format!("{source}/contents/upstream"),
        upstream_token,
        json!({"message":"Upstream advance","content":"dXBzdHJlYW0="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    let upstream_sha = advance["commit"]["sha"].as_str().unwrap();
    assert_eq!(
        call(
            &router,
            "POST",
            &endpoint,
            writer,
            json!({"branch":"main"}),
            ("repos/merge-upstream", 200)
        )
        .await["merge_type"],
        "fast-forward"
    );
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("{target}/git/ref/heads/main"),
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        upstream_sha
    );
    let fork_change = call(
        &router,
        "PUT",
        &format!("{target}/contents/local"),
        writer,
        json!({"message":"Fork change","content":"bG9jYWw="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "POST",
            &endpoint,
            writer,
            json!({"branch":"main"}),
            ("repos/merge-upstream", 200)
        )
        .await["merge_type"],
        "none"
    );
    let next = call(
        &router,
        "PUT",
        &format!("{source}/contents/next"),
        upstream_token,
        json!({"message":"Next upstream change","content":"bmV4dA=="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "POST",
            &endpoint,
            writer,
            json!({"branch":"main"}),
            ("repos/merge-upstream", 200)
        )
        .await["merge_type"],
        "merge"
    );
    let merged = call(
        &router,
        "GET",
        &format!("{target}/commits/main"),
        reader,
        Value::Null,
        ("repos/get-commit", 200),
    )
    .await;
    assert_eq!(merged["parents"][0]["sha"], fork_change["commit"]["sha"]);
    assert_eq!(merged["parents"][1]["sha"], next["commit"]["sha"]);
    for file in ["local", "upstream", "next"] {
        call(
            &router,
            "GET",
            &format!("{target}/contents/{file}"),
            reader,
            Value::Null,
            ("repos/get-content", 200),
        )
        .await;
    }
    // Conflicts leave the fork tip unchanged, including commits from its earlier sync.
    let local = call(
        &router,
        "PUT",
        &format!("{target}/contents/conflict"),
        writer,
        json!({"message":"Local conflict","content":"bG9jYWw="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    call(
        &router,
        "PUT",
        &format!("{source}/contents/conflict"),
        upstream_token,
        json!({"message":"Upstream conflict","content":"cmVtb3Rl"}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    call(
        &router,
        "POST",
        &endpoint,
        writer,
        json!({"branch":"main"}),
        ("repos/merge-upstream", 409),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("{target}/git/ref/heads/main"),
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        local["commit"]["sha"]
    );
    protected_sync(&router, source, target, upstream_token, writer, seed_sha).await;
}

async fn protected_sync(
    router: &Router,
    source: &str,
    target: &str,
    upstream_token: &str,
    writer: &str,
    seed: &str,
) {
    for base in [source, target] {
        let token = if base == source {
            upstream_token
        } else {
            writer
        };
        call(
            router,
            "POST",
            &format!("{base}/git/refs"),
            token,
            json!({"ref":"refs/heads/protected","sha":seed}),
            ("git/create-ref", 201),
        )
        .await;
    }
    call(
        router,
        "PUT",
        &format!("{source}/contents/advance"),
        upstream_token,
        json!({"message":"Protected sync source","branch":"protected","content":"eA=="}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    call(router,"PUT",&format!("{target}/branches/protected/protection"),writer,json!({"required_status_checks":null,"enforce_admins":true,"required_pull_request_reviews":{},"restrictions":null}),("repos/update-branch-protection",200)).await;
    call(
        router,
        "POST",
        &format!("{target}/merge-upstream"),
        writer,
        json!({"branch":"protected"}),
        ("repos/merge-upstream", 403),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &format!("{target}/git/ref/heads/protected"),
            writer,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        seed
    );
    call(
        router,
        "POST",
        &format!("{target}/git/refs"),
        writer,
        json!({"ref":"refs/heads/local-only","sha":seed}),
        ("git/create-ref", 201),
    )
    .await;
    call(
        router,
        "POST",
        &format!("{target}/merge-upstream"),
        writer,
        json!({"branch":"local-only"}),
        ("repos/merge-upstream", 422),
    )
    .await;
}

async fn commit(
    router: &Router,
    base: &str,
    token: &str,
    tree: &str,
    parent: &str,
    message: &str,
) -> String {
    call(
        router,
        "POST",
        &format!("{base}/git/commits"),
        token,
        json!({"message":message,"tree":tree,"parents":[parent]}),
        ("git/create-commit", 201),
    )
    .await["sha"]
        .as_str()
        .unwrap()
        .into()
}

async fn tree(
    router: &Router,
    base: &str,
    token: &str,
    parent_tree: &str,
    contents: &str,
) -> String {
    let blob = call(
        router,
        "POST",
        &format!("{base}/git/blobs"),
        token,
        json!({"content":contents}),
        ("git/create-blob", 201),
    )
    .await;
    call(router, "POST", &format!("{base}/git/trees"), token, json!({"base_tree":parent_tree,"tree":[{"path":"merge-file.txt","mode":"100644","type":"blob","sha":blob["sha"]}]}), ("git/create-tree", 201)).await["sha"].as_str().unwrap().into()
}

pub(super) async fn run(
    router: &Router,
    resource: (&db::DbPool, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
    session: &Client,
) {
    let (pool, repo, alice) = resource;
    let (writer, reader, outsider) = tokens;
    let original = call(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    let original_tree = original["tree"]["sha"].as_str().unwrap();
    let head_tree = tree(router, base, writer, original_tree, "head contents\n").await;
    let base_tree = tree(router, base, writer, original_tree, "base contents\n").await;
    let head = commit(router, base, writer, &head_tree, sha, "Merge source").await;
    let conflicting = commit(router, base, writer, &base_tree, sha, "Conflicting base").await;
    for (name, target) in [
        ("merge-base", sha),
        ("merge-head", &head),
        ("merge-conflict", &conflicting),
    ] {
        call(
            router,
            "POST",
            &format!("{base}/git/refs"),
            writer,
            json!({"ref":format!("refs/heads/{name}"),"sha":target}),
            ("git/create-ref", 201),
        )
        .await;
    }
    let path = format!("{base}/merges");
    let body = json!({"base":"merge-base","head":"merge-head","commit_message":"Merge with a real two-parent commit"});
    call(
        router,
        "POST",
        &path,
        reader,
        body.clone(),
        ("repos/merge", 403),
    )
    .await;
    call(
        router,
        "POST",
        &path,
        outsider,
        body.clone(),
        ("repos/merge", 404),
    )
    .await;
    for body in [
        json!({"base":"missing","head":"merge-head"}),
        json!({"base":"merge-base","head":"missing"}),
    ] {
        call(router, "POST", &path, writer, body, ("repos/merge", 404)).await;
    }
    call(
        router,
        "POST",
        &path,
        writer,
        json!({"base":"bad..name","head":"merge-head"}),
        ("repos/merge", 422),
    )
    .await;
    call(
        router,
        "POST",
        &path,
        writer,
        json!({"base":"merge-base","head":"merge-head","commit_message":"bad\0message"}),
        ("repos/merge", 422),
    )
    .await;
    call(
        router,
        "POST",
        &path,
        writer,
        json!({"base":"merge-conflict","head":"merge-head"}),
        ("repos/merge", 409),
    )
    .await;
    let merged = call(router, "POST", &path, writer, body, ("repos/merge", 201)).await;
    let merged_sha = merged["sha"].as_str().unwrap();
    assert_eq!(merged["parents"][0]["sha"], sha);
    assert_eq!(merged["parents"][1]["sha"], head);
    assert_eq!(
        merged["commit"]["message"],
        "Merge with a real two-parent commit"
    );
    assert_eq!(merged["commit"]["verification"]["reason"], "unsigned");
    let reference = format!("{base}/git/ref/heads/merge-base");
    assert_eq!(
        call(
            router,
            "GET",
            &reference,
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        merged_sha
    );
    call(
        router,
        "POST",
        &path,
        writer,
        json!({"base":"merge-base","head":"merge-head"}),
        ("repos/merge", 204),
    )
    .await;
    call(
        router,
        "POST",
        &path,
        writer,
        json!({"base":"merge-base","head":head}),
        ("repos/merge", 204),
    )
    .await;

    let next_head = commit(
        router,
        base,
        writer,
        &head_tree,
        &head,
        "Another source commit",
    )
    .await;
    call(
        router,
        "PATCH",
        &format!("{base}/git/refs/heads/merge-head"),
        writer,
        json!({"sha":next_head}),
        ("git/update-ref", 200),
    )
    .await;
    db::repositories::protection::upsert_branch_protection_rule(
        pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: alice,
            repo_id: repo,
            pattern: "merge-base",
            require_pull_request: true,
            enforce_admins: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    call(
        router,
        "POST",
        &path,
        writer,
        json!({"base":"merge-base","head":"merge-head"}),
        ("repos/merge", 403),
    )
    .await;
    assert_eq!(
        call(
            router,
            "GET",
            &reference,
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        merged_sha,
        "a branch merge must not claim pull-request approval authority"
    );
    call(
        router,
        "DELETE",
        &format!("{base}/branches/merge-base/protection"),
        writer,
        Value::Null,
        ("repos/delete-branch-protection", 204),
    )
    .await;

    let native = format!("{}/merge", base.strip_prefix("/api/v3/repos").unwrap());
    let uri = format!("{native}?base=merge-base&head=merge-head");
    let (status, html) = session.page(&uri).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Merge branches"));
    assert!(html.contains(merged_sha));
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&uri)
                .header(
                    header::ACCEPT,
                    codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
                )
                .header(header::COOKIE, format!("session_id={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let model: codeyard::webui::app::PageEnvelope = serde_json::from_slice(&bytes).unwrap();
    let codeyard::webui::app::Page::Repositories(
        codeyard::webui::pages::repos::page::Page::BranchMerge { form, .. },
    ) = model.page
    else {
        panic!("merge page model")
    };
    assert_eq!(form.base, format!("{merged_sha} merge-base"));
    assert_eq!(form.head, format!("{next_head} merge-head"));
    assert_eq!(form.repository_id, repo.to_string());
    assert!(form.can_merge);
    assert!(!String::from_utf8_lossy(&bytes).contains("password_hash"));
    let repo_id = repo.to_string();
    let base_choice = format!("{merged_sha} merge-base");
    let head_choice = format!("{next_head} merge-head");
    let old_choice = format!("{sha} merge-base");
    let (status, html) = session
        .form(
            &native,
            &[
                ("repository_id", &repo_id),
                ("base", &old_choice),
                ("head", &head_choice),
                ("message", "Preserved stale merge message <script>"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert!(html.contains("Preserved stale merge message &lt;script&gt;"));
    let wrong_repo = (repo + 10_000).to_string();
    let (status, html) = session
        .form(
            &native,
            &[
                ("repository_id", &wrong_repo),
                ("base", &base_choice),
                ("head", &head_choice),
                ("message", "Wrong repository draft"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert!(html.contains("Wrong repository draft"));
    let merge_form = html
        .split_once(&format!("action=\"{native}\""))
        .expect("the rejected merge form")
        .1
        .split_once("</form>")
        .unwrap()
        .0;
    assert_eq!(
        crate::support::http_client::field_value(merge_form, "repository_id"),
        Some(wrong_repo)
    );
    let bad_csrf = session
        .form_reply(
            &native,
            &[
                ("csrf_token", "bad"),
                ("repository_id", &repo_id),
                ("base", &base_choice),
                ("head", &head_choice),
            ],
        )
        .await;
    assert_eq!(bad_csrf.status, StatusCode::FORBIDDEN);
    assert_eq!(
        call(
            router,
            "GET",
            &reference,
            reader,
            Value::Null,
            ("git/get-ref", 200)
        )
        .await["object"]["sha"],
        merged_sha
    );
    let success = session
        .form_reply(
            &native,
            &[
                ("repository_id", &repo_id),
                ("base", &base_choice),
                ("head", &head_choice),
                ("message", "Merged through a native form"),
            ],
        )
        .await;
    assert_eq!(success.status, StatusCode::SEE_OTHER, "{}", success.text());
    let now = call(
        router,
        "GET",
        &reference,
        reader,
        Value::Null,
        ("git/get-ref", 200),
    )
    .await;
    let final_sha = now["object"]["sha"].as_str().unwrap();
    assert!(success
        .location
        .as_deref()
        .unwrap()
        .ends_with(&format!("/commit/{final_sha}")));
    let info = call(
        router,
        "GET",
        &format!("{base}/git/commits/{final_sha}"),
        reader,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    assert_eq!(info["message"], "Merged through a native form");
    assert_eq!(info["parents"][0]["sha"], merged_sha);
    assert_eq!(info["parents"][1]["sha"], next_head);
    for name in ["merge-base", "merge-head", "merge-conflict"] {
        call(
            router,
            "DELETE",
            &format!("{base}/git/refs/heads/{name}"),
            writer,
            Value::Null,
            ("git/delete-ref", 204),
        )
        .await;
    }
}
