use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};

async fn text(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    content_type: &str,
    input: &str,
) -> (HeaderMap, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", content_type);
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(input.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    (headers, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let mut credentials = Vec::new();
    for scope in ["write", "read"] {
        let issued = tokens::create(
            f.pool(),
            alice,
            Some(&session.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", issued.value));
    }
    let writer = &credentials[0];
    let reader = &credentials[1];
    let (_, initial) = checked(
        &router,
        "rate-limit/get",
        "GET",
        "/api/v3/rate_limit",
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(initial["rate"]["used"], 0);
    for (op, path) in [
        ("meta/root", "/api/v3"),
        ("meta/get", "/api/v3/meta"),
        ("meta/get-all-versions", "/api/v3/versions"),
        (
            "licenses/get-all-commonly-used",
            "/api/v3/licenses?per_page=1",
        ),
        (
            "codes-of-conduct/get-all-codes-of-conduct",
            "/api/v3/codes_of_conduct",
        ),
        (
            "codes-of-conduct/get-conduct-code",
            "/api/v3/codes_of_conduct/contributor_covenant",
        ),
        ("gitignore/get-all-templates", "/api/v3/gitignore/templates"),
        ("gitignore/get-template", "/api/v3/gitignore/templates/Rust"),
        (
            "gitignore/get-template",
            "/api/v3/gitignore/templates/C%2B%2B",
        ),
        ("emojis/get", "/api/v3/emojis"),
    ] {
        checked(&router, op, "GET", path, Some(writer), Value::Null, 200).await;
    }
    checked(
        &router,
        "meta/root",
        "GET",
        "/api/v3/",
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    for key in [
        "mit",
        "apache-2.0",
        "gpl-3.0",
        "bsd-3-clause",
        "mpl-2.0",
        "unlicense",
    ] {
        let (_, license) = checked(
            &router,
            "licenses/get",
            "GET",
            &format!("/api/v3/licenses/{key}"),
            None,
            Value::Null,
            200,
        )
        .await;
        assert!(!license["body"].as_str().unwrap().is_empty());
    }
    for (op, path) in [
        ("licenses/get", "/api/v3/licenses/missing"),
        (
            "codes-of-conduct/get-conduct-code",
            "/api/v3/codes_of_conduct/missing",
        ),
        (
            "gitignore/get-template",
            "/api/v3/gitignore/templates/missing",
        ),
    ] {
        checked(&router, op, "GET", path, Some(reader), Value::Null, 404).await;
    }
    let (headers, html) = text(
        &router,
        "POST",
        "/api/v3/markdown",
        Some(writer),
        "application/json",
        r##"{"text":"# Hello\n\n:rocket: <script>alert(1)</script>","mode":"gfm"}"##,
    )
    .await;
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert!(html.contains("Hello"));
    assert!(html.contains('🚀'));
    assert!(!html.contains("<script>"));
    let (_, raw) = text(
        &router,
        "POST",
        "/api/v3/markdown/raw",
        Some(reader),
        "text/plain",
        "**bold**",
    )
    .await;
    assert!(raw.contains("<strong class=\"font-semibold text-foreground\">bold</strong>"));
    let (_, zen) = text(&router, "GET", "/api/v3/zen", None, "text/plain", "").await;
    assert!(!zen.is_empty());
    let (_, cat) = text(
        &router,
        "GET",
        "/api/v3/octocat?s=Hi",
        None,
        "text/plain",
        "",
    )
    .await;
    assert!(cat.contains("Hi"));
    let (_, svg) = text(
        &router,
        "GET",
        "/api/v3/emojis/rocket",
        None,
        "text/plain",
        "",
    )
    .await;
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains('🚀'));
    codeyard_app::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let license_text = codeyard_app::repositories::templates::fill_license(
        codeyard_app::repositories::templates::license("MIT")
            .unwrap()
            .text,
        2026,
        "Alice",
    );
    checked(
        &router,
        "repos/create-or-update-file-contents",
        "PUT",
        "/api/v3/repos/alice/private/contents/LICENSE",
        Some(writer),
        json!({"message":"License","content":STANDARD.encode(&license_text)}),
        201,
    )
    .await;
    let (_, license) = checked(
        &router,
        "licenses/get-for-repo",
        "GET",
        "/api/v3/repos/alice/private/license",
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(license["license"]["spdx_id"], "MIT");
    assert_eq!(
        STANDARD
            .decode(license["content"].as_str().unwrap())
            .unwrap(),
        license_text.as_bytes()
    );
    checked(
        &router,
        "licenses/get-for-repo",
        "GET",
        "/api/v3/repos/alice/private/license",
        None,
        Value::Null,
        404,
    )
    .await;
    checked(
        &router,
        "markdown/render",
        "POST",
        "/api/v3/markdown",
        None,
        json!({"text":"#1","context":"alice/private","mode":"gfm"}),
        404,
    )
    .await;
    let (_, before) = checked(
        &router,
        "rate-limit/get",
        "GET",
        "/api/v3/rate_limit",
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    checked(
        &router,
        "meta/get",
        "GET",
        "/api/v3/meta",
        Some(writer),
        Value::Null,
        200,
    )
    .await;
    let (_, after) = checked(
        &router,
        "rate-limit/get",
        "GET",
        "/api/v3/rate_limit",
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        after["rate"]["used"].as_i64().unwrap(),
        before["rate"]["used"].as_i64().unwrap() + 1
    );
    f.execute(
        "UPDATE github_api_limits SET used=5000 WHERE resource='core'",
        &[],
    )
    .await;
    let (headers, exhausted) = checked(
        &router,
        "rate-limit/get",
        "GET",
        "/api/v3/rate_limit",
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(exhausted["rate"]["remaining"], 0);
    assert_eq!(headers["x-ratelimit-remaining"], "0");
    checked(
        &router,
        "meta/get",
        "GET",
        "/api/v3/meta",
        Some(writer),
        Value::Null,
        403,
    )
    .await;
    let concurrent = futures_util::future::join_all((0..20).map(|_| {
        db::api_limits::budget(
            f.pool(),
            db::api_limits::Request {
                key: "concurrent",
                resource: "search",
                now: 100,
                limit: 5,
                seconds: 60,
                consume: true,
            },
        )
    }))
    .await;
    let concurrent: Vec<_> = concurrent.into_iter().map(Result::unwrap).collect();
    assert_eq!(concurrent.iter().filter(|(_, allowed)| *allowed).count(), 5);
    assert!(concurrent
        .iter()
        .all(|(budget, _)| budget.used <= 5 && budget.reset_at == 160));
    let (first, accepted) = db::api_limits::budget(
        f.pool(),
        db::api_limits::Request {
            key: "test",
            resource: "core",
            now: 100,
            limit: 1,
            seconds: 60,
            consume: true,
        },
    )
    .await
    .unwrap();
    assert!(accepted);
    assert_eq!(first.used, 1);
    let (_, accepted) = db::api_limits::budget(
        f.pool(),
        db::api_limits::Request {
            key: "test",
            resource: "core",
            now: 100,
            limit: 1,
            seconds: 60,
            consume: true,
        },
    )
    .await
    .unwrap();
    assert!(!accepted);
    let (_, accepted) = db::api_limits::budget(
        f.pool(),
        db::api_limits::Request {
            key: "test",
            resource: "core",
            now: 161,
            limit: 1,
            seconds: 60,
            consume: true,
        },
    )
    .await
    .unwrap();
    assert!(accepted);
}
#[tokio::test]
async fn sqlite_github_meta() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_meta() {
    contract(true).await;
}
