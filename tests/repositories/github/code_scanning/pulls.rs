use super::{call, db, json, sarif, Client, Router, Value};
use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::code_scanning;

pub(super) async fn contract(
    router: &Router,
    pool: &db::DbPool,
    repo: i64,
    actor: i64,
    token: &str,
    base: &str,
) {
    crate::support::fake_webui_bundle::install();
    let endpoint = "/api/v3/repos/alice/scan";
    call(
        router,
        "POST",
        &format!("{endpoint}/git/refs"),
        token,
        json!({"ref":"refs/heads/scanning-pr","sha":base}),
        "git/create-ref",
        201,
    )
    .await;
    let file = call(
        router,
        "GET",
        &format!("{endpoint}/contents/README.md"),
        token,
        Value::Null,
        "repos/get-content",
        200,
    )
    .await;
    let saved=call(router,"PUT",&format!("{endpoint}/contents/README.md"),token,json!({"message":"Add unsafe call","branch":"scanning-pr","sha":file["sha"],"content":STANDARD.encode(b"unsafe();\n")}),"repos/create-or-update-file-contents",200).await;
    let head = saved["commit"]["sha"].as_str().unwrap();
    let pull = db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
            repo_id: repo,
            head_repo_id: Some(repo),
            author_id: actor,
            issue: None,
            title: "Scanning annotations",
            body: "",
            head_branch: "scanning-pr",
            head_sha: head,
            base_branch: "main",
            base_sha: base,
            is_draft: false,
            maintainer_can_modify: false,
        },
    )
    .await
    .unwrap();
    let reference = format!("refs/pull/{}/head", pull.number);
    let scans = format!("{endpoint}/code-scanning/sarifs");
    call(
        router,
        "POST",
        &scans,
        token,
        json!({"ref":"refs/heads/main","commit_sha":base,"sarif":sarif("pr-scanner/",false,1)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(code_scanning::process_next(pool).await.unwrap());
    call(
        router,
        "POST",
        &scans,
        token,
        json!({"ref":reference,"commit_sha":head,"sarif":sarif("pr-scanner/",true,1)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    let client = Client::login(router, pool, actor).await;
    let checks = format!("/alice/scan/pulls/{}/checks", pull.number);
    let (status, html) = client.send("GET", &checks, "", String::new()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("SARIF upload"));
    assert!(html.contains("pending"));
    assert!(code_scanning::process_next(pool).await.unwrap());
    let (status, html) = client.send("GET", &checks, "", String::new()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("1 new findings compared with the base revision."),
        "{html}"
    );
    assert_eq!(
        html.matches("aria-label=\"Code scanning annotation\"")
            .count(),
        1
    );
    for suffix in ["diff", "diff?view=split"] {
        let (status, html) = client
            .send(
                "GET",
                &format!("/alice/scan/pulls/{}/{suffix}", pull.number),
                "",
                String::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            html.matches("aria-label=\"Code scanning annotation\"")
                .count(),
            1,
            "{html}"
        );
        assert!(html.contains("README.md:1"));
    }
    assert!(db::code_scanning::pull_analyses(
        pool,
        repo,
        &"f".repeat(40),
        pull.number,
        Some("refs/heads/scanning-pr")
    )
    .await
    .unwrap()
    .is_empty());
    // An identical fingerprint on the exact base is not a newly introduced PR finding.
    call(
        router,
        "POST",
        &scans,
        token,
        json!({"ref":"refs/heads/main","commit_sha":base,"sarif":sarif("pr-scanner/",true,1)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(code_scanning::process_next(pool).await.unwrap());
    let (status, html) = client.send("GET", &checks, "", String::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("0 new findings compared with the base revision."),
        "{html}"
    );
    assert!(!html.contains("aria-label=\"Code scanning annotation\""));
    // No baseline is neutral, not an invented passing comparison.
    call(
        router,
        "POST",
        &scans,
        token,
        json!({"ref":reference,"commit_sha":head,"sarif":sarif("missing-baseline/",true,1)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(code_scanning::process_next(pool).await.unwrap());
    let (_, html) = client.send("GET", &checks, "", String::new()).await;
    assert!(html.contains("No matching analysis exists for the base revision"));
    assert!(html.contains("neutral"));
    let bob = db::users::accounts::get_user_by_username(pool, "bob")
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        db::repositories::access::change_personal_repository_collaborator(
            pool,
            actor,
            repo,
            "bob",
            Some(db::models::AccessLevel::Read)
        )
        .await
        .unwrap(),
        db::repositories::access::CollaboratorChange::Applied(_)
    ));
    let reader = Client::login(router, pool, bob.id).await;
    let (status, html) = reader.send("GET", &checks, "", String::new()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(!html.contains("aria-label=\"Code scanning checks\""));
    assert!(!html.contains("aria-label=\"Code scanning annotation\""));
    assert!(matches!(
        db::repositories::access::change_personal_repository_collaborator(
            pool, actor, repo, "bob", None
        )
        .await
        .unwrap(),
        db::repositories::access::CollaboratorChange::Applied(_)
    ));
}
