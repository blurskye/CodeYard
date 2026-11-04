use super::*;
use crate::support::git_server::{self, Server};
use axum::extract::ConnectInfo;
use chrono::Datelike;

use std::net::SocketAddr;
#[derive(Clone, Copy)]
enum Mode {
    Html,
    Navigation,
    Poll,
}
async fn navigation(
    router: &Router,
    session: Option<&str>,
    path: &str,
    method: &str,
    mode: Mode,
    address: &str,
    referrer: Option<&str>,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("user-agent", "Traffic browser")
        .header("x-codeyard-client", "203.0.113.250")
        .extension(ConnectInfo(address.parse::<SocketAddr>().unwrap()));
    if let Some(session) = session {
        request = request.header("cookie", format!("session_id={session}"));
    }
    if !matches!(mode, Mode::Html) {
        request = request.header("accept", "application/vnd.codeyard.page+json");
    }
    if matches!(mode, Mode::Navigation) {
        request = request.header("x-codeyard-navigation", "navigate");
    }
    if let Some(referrer) = referrer {
        request = request.header("referer", referrer);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}
async fn views(router: &Router, token: &str) -> Value {
    checked(
        router,
        "repos/get-views",
        "GET",
        "/api/v3/repos/alice/proj/traffic/views",
        Some(token),
        Value::Null,
        StatusCode::OK,
    )
    .await
    .1
}
async fn clones(router: &Router, token: &str) -> Value {
    checked(
        router,
        "repos/get-clones",
        "GET",
        "/api/v3/repos/alice/proj/traffic/clones",
        Some(token),
        Value::Null,
        StatusCode::OK,
    )
    .await
    .1
}
async fn traffic_contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(&root.path().join("repos")).await;
    let writer = credential(&router, &f, alice, "write").await;
    let reader = credential(&router, &f, alice, "read").await;
    let bob_token = credential(&router, &f, bob, "read").await;
    let outsider = credential(&router, &f, carol, "write").await;
    let (_, created) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"proj","private":true,"auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let repo = f.repository(created["id"].as_i64().unwrap()).await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let owner = Client::login(&router, f.pool(), alice).await;
    let visitor = Client::login(&router, f.pool(), bob).await;
    assert_eq!(
        views(&router, &reader).await,
        json!({"count":0,"uniques":0,"views":[]})
    );
    assert_eq!(
        clones(&router, &reader).await,
        json!({"count":0,"uniques":0,"clones":[]})
    );
    for (operation, path) in [
        ("repos/get-views", "views"),
        ("repos/get-clones", "clones"),
        ("repos/get-top-paths", "popular/paths"),
        ("repos/get-top-referrers", "popular/referrers"),
    ] {
        let endpoint = format!("/api/v3/repos/alice/proj/traffic/{path}");
        for token in [None, Some(outsider.as_str())] {
            checked(
                &router,
                operation,
                "GET",
                &endpoint,
                token,
                Value::Null,
                StatusCode::NOT_FOUND,
            )
            .await;
        }
        checked(
            &router,
            operation,
            "GET",
            &endpoint,
            Some(&bob_token),
            Value::Null,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    let address = "192.0.2.1:12345";
    let page = "/alice/proj/issues";
    for mode in [Mode::Html, Mode::Navigation] {
        let (status, body) = navigation(
            &router,
            Some(&owner.session),
            page,
            "GET",
            mode,
            address,
            Some("https://search.example/path?secret_token_marker=hidden"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    for (path, method, mode) in [
        (page, "GET", Mode::Poll),
        (page, "HEAD", Mode::Html),
        ("/alice/proj/settings", "GET", Mode::Html),
        ("/alice/proj/security", "GET", Mode::Html),
        ("/alice/proj/missing-route", "GET", Mode::Html),
    ] {
        let _ = navigation(
            &router,
            Some(&owner.session),
            path,
            method,
            mode,
            address,
            None,
        )
        .await;
    }
    let (status, _) = navigation(&router, None, page, "GET", Mode::Html, address, None).await;
    assert!(!status.is_success());
    assert_eq!(views(&router, &reader).await["count"], 2);
    let (status, body) = navigation(
        &router,
        Some(&visitor.session),
        page,
        "GET",
        Mode::Html,
        address,
        Some("https://u:p@news.example/secret?secret_token_marker=hidden"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let self_referrer = created["html_url"].as_str().unwrap();
    let (status, body) = navigation(
        &router,
        Some(&owner.session),
        "/alice/proj/issues?page=1",
        "GET",
        Mode::Html,
        address,
        Some(self_referrer),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let observed = views(&router, &reader).await;
    assert_eq!(observed["count"], 4);
    assert_eq!(observed["uniques"], 2);
    assert_eq!(observed["views"].as_array().unwrap().len(), 1);
    let (_, paths) = checked(
        &router,
        "repos/get-top-paths",
        "GET",
        "/api/v3/repos/alice/proj/traffic/popular/paths",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(paths.as_array().unwrap().len(), 1);
    assert_eq!(paths[0]["path"], page);
    assert_eq!(paths[0]["count"], 4);
    assert_eq!(paths[0]["uniques"], 2);
    let (_, referrers) = checked(
        &router,
        "repos/get-top-referrers",
        "GET",
        "/api/v3/repos/alice/proj/traffic/popular/referrers",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        referrers[0],
        json!({"referrer":"search.example","count":2,"uniques":1})
    );
    assert_eq!(referrers[1]["referrer"], "news.example");
    assert!(!referrers.to_string().contains("secret_token_marker"));
    for n in 0..11 {
        let title = if n == 0 {
            "<script>alert('traffic')</script>".to_owned()
        } else {
            format!("Traffic issue {n}")
        };
        let issue = owner
            .expect(
                "POST",
                "/api/repos/alice/proj/issues",
                json!({"title":title,"body":"Actual repository content"}),
                StatusCode::CREATED,
            )
            .await;
        let path = format!("/alice/proj/issues/{}", issue["number"].as_i64().unwrap());
        let source = format!("https://r{n}.example/private?secret_token_marker=hidden");
        let (status, body) = navigation(
            &router,
            Some(&owner.session),
            &path,
            "GET",
            Mode::Navigation,
            address,
            Some(&source),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    for (operation, path) in [
        ("repos/get-top-paths", "paths"),
        ("repos/get-top-referrers", "referrers"),
    ] {
        let (_, data) = checked(
            &router,
            operation,
            "GET",
            &format!("/api/v3/repos/alice/proj/traffic/popular/{path}"),
            Some(&reader),
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(data.as_array().unwrap().len(), 10);
        assert!(!data.to_string().contains("secret_token_marker"));
    }
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
    // The forged private address header is the same for both real addresses. It must not
    // collapse them, nor invent a new visitor when the same address returns.
    for address in ["192.0.2.2:12345", "192.0.2.2:54321", "192.0.2.3:12345"] {
        let (status, body) =
            navigation(&router, None, page, "GET", Mode::Html, address, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let observed = views(&router, &reader).await;
    assert_eq!(observed["count"], 18);
    assert_eq!(observed["uniques"], 4);
    for token in [None, Some(outsider.as_str())] {
        checked(
            &router,
            "repos/get-views",
            "GET",
            "/api/v3/repos/alice/proj/traffic/views",
            token,
            Value::Null,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    assert_eq!(views(&router, &bob_token).await["count"], 18);
    db::repositories::access::revoke_repo_access(f.pool(), bob, repo.id)
        .await
        .unwrap();
    checked(
        &router,
        "repos/get-views",
        "GET",
        "/api/v3/repos/alice/proj/traffic/views",
        Some(&bob_token),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    checked(
        &router,
        "repos/get-views",
        "GET",
        "/api/v3/repos/alice/proj/traffic/views?per=fortnight",
        Some(&reader),
        Value::Null,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    // Move actual observed events to last Monday, retaining each old day key and matching
    // fingerprints. The returning account must retain one UUID across daily and weekly bins.
    let today = chrono::Utc::now().date_naive();
    let yesterday = today - chrono::Duration::days(1);
    let monday =
        today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()) + 7);
    let sunday = monday - chrono::Duration::days(1);
    let tuesday = monday + chrono::Duration::days(1);
    for table in [
        "repository_traffic_keys",
        "repository_traffic_visitors",
        "repository_traffic_counts",
    ] {
        f.execute(&format!("UPDATE {table} SET day=CASE WHEN day='{today}' THEN '{monday}' WHEN day='{yesterday}' THEN '{sunday}' ELSE day END WHERE repo_id=$1"),&[repo.id]).await;
    }
    let (status, body) = navigation(
        &router,
        Some(&owner.session),
        page,
        "GET",
        Mode::Navigation,
        address,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    f.execute(&format!("UPDATE repository_traffic_counts SET day='{tuesday}' WHERE repo_id=$1 AND day='{today}' AND kind='view'"),&[repo.id]).await;
    let daily = views(&router, &reader).await;
    assert_eq!(daily["count"], 19);
    assert_eq!(daily["uniques"], 4);
    assert_eq!(daily["views"].as_array().unwrap().len(), 2);
    assert_eq!(
        daily["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["uniques"].as_i64().unwrap())
            .sum::<i64>(),
        5
    );
    let (_, weekly) = checked(
        &router,
        "repos/get-views",
        "GET",
        "/api/v3/repos/alice/proj/traffic/views?per=week",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(weekly["count"], 19);
    assert_eq!(weekly["uniques"], 4);
    assert_eq!(weekly["views"].as_array().unwrap().len(), 1);
    assert_eq!(weekly["views"][0]["uniques"], 4);
    assert_eq!(
        weekly["views"][0]["timestamp"],
        format!("{monday}T00:00:00Z")
    );
    // Privacy cleanup applies to quiet repositories too, independently of a new observation.
    f.execute("INSERT INTO repository_traffic_keys(repo_id,day,salt) SELECT repo_id,'2000-01-01',salt FROM repository_traffic_keys WHERE repo_id=$1 LIMIT 1",&[repo.id]).await;
    f.execute("INSERT INTO repository_traffic_visitors(repo_id,day,fingerprint,visitor) SELECT repo_id,'2000-01-01',fingerprint,visitor FROM repository_traffic_visitors WHERE repo_id=$1 LIMIT 1",&[repo.id]).await;
    f.execute("INSERT INTO repository_traffic_counts(repo_id,day,kind,visitor,path_hash,path,title,referrer,count,last_seen) SELECT repo_id,'2000-01-01',kind,visitor,path_hash,path,title,referrer,count,last_seen FROM repository_traffic_counts WHERE repo_id=$1 LIMIT 1",&[repo.id]).await;
    assert_eq!(views(&router, &reader).await["count"], 19);
    assert!(
        codeyard::backend::repositories::traffic::purge_expired(f.pool())
            .await
            .unwrap()
            >= 3
    );
    for table in [
        "repository_traffic_keys",
        "repository_traffic_visitors",
        "repository_traffic_counts",
    ] {
        assert_eq!(
            f.scalar(
                &format!("SELECT COUNT(*) FROM {table} WHERE day='2000-01-01'"),
                &[]
            )
            .await,
            0
        );
    }
    let server = Server::start(root.path(), &f.connection_url()).await;
    let token = git_server::write_token(f.pool(), alice).await;
    let remote = format!("{}/alice/proj.git", server.base);
    git_server::success(git_server::git(
        root.path(),
        &["ls-remote", &remote],
        Some(&token),
    ));
    assert_eq!(clones(&router, &reader).await["count"], 0);
    git_server::success(git_server::git(
        root.path(),
        &["clone", "-q", &remote, "work"],
        Some(&token),
    ));
    assert_eq!(clones(&router, &reader).await["count"], 1);
    let work = root.path().join("work");
    let head = git2::Repository::open_bare(&repo.disk_path)
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap()
        .to_string();
    owner
        .save_text(
            "main",
            "traffic.txt",
            "An actual new object for a negotiated fetch\n",
            &head,
        )
        .await;
    git_server::success(git_server::git(
        &work,
        &["fetch", "-q", "origin"],
        Some(&token),
    ));
    assert_eq!(clones(&router, &reader).await["count"], 1);
    git_server::success(git_server::git(
        root.path(),
        &["clone", "-q", &remote, "second"],
        Some(&token),
    ));
    let observed = clones(&router, &reader).await;
    assert_eq!(observed["count"], 2);
    assert_eq!(observed["uniques"], 1);
    git_server::success(git_server::git(
        root.path(),
        &["clone", "-q", &remote, "anonymous"],
        None,
    ));
    let observed = clones(&router, &reader).await;
    assert_eq!(observed["count"], 3);
    assert_eq!(observed["uniques"], 2);
    let (_, weekly) = checked(
        &router,
        "repos/get-clones",
        "GET",
        "/api/v3/repos/alice/proj/traffic/clones?per=week",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(weekly["count"], 3);
    assert_eq!(weekly["uniques"], 2);
    let (status, html) = navigation(
        &router,
        Some(&owner.session),
        "/alice/proj/insights/traffic?per=week",
        "GET",
        Mode::Html,
        address,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Initial Git object transfers") && html.contains("name=\"per\""));
    assert!(!html.contains("<script>alert('traffic')</script>"));
    assert!(!html.contains("secret_token_marker"));
    let (status, _) = navigation(
        &router,
        Some(&visitor.session),
        "/alice/proj/insights/traffic",
        "GET",
        Mode::Html,
        address,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/proj",
        Some(&writer),
        json!({"archived":true}),
        StatusCode::OK,
    )
    .await;
    let (status, html) = navigation(
        &router,
        Some(&visitor.session),
        "/alice/proj/insights/traffic",
        "GET",
        Mode::Html,
        address,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("href=\"/alice/proj/insights/traffic\""));
    let personal = views(&router, &reader).await["count"].as_i64().unwrap();
    let (_, other) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"other","auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let (status, body) = navigation(
        &router,
        Some(&owner.session),
        "/alice/other/issues",
        "GET",
        Mode::Html,
        address,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, data) = checked(
        &router,
        "repos/get-views",
        "GET",
        "/api/v3/repos/alice/other/traffic/views",
        Some(&reader),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(data["count"], 1);
    assert_eq!(views(&router, &reader).await["count"], personal);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM repository_traffic_keys a JOIN repository_traffic_keys b ON a.salt=b.salt WHERE a.repo_id=$1 AND b.repo_id=$2",&[repo.id,other["id"].as_i64().unwrap()]).await,0);
    use db::organizations::{membership, profiles, OrganizationOutcome};
    let input = profiles::NewOrganization::new(
        "acme",
        profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let OrganizationOutcome::Applied(org) = profiles::create_organization(f.pool(), alice, &input)
        .await
        .unwrap()
    else {
        panic!("organization")
    };
    assert!(matches!(
        membership::add_organization_member(
            f.pool(),
            alice,
            org.id,
            bob,
            codeyard_db_core::models::OrgRole::Member
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    let (_, org_repo) = checked(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        Some(&writer),
        json!({"name":"metrics"}),
        StatusCode::CREATED,
    )
    .await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        org_repo["id"].as_i64().unwrap(),
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let org_endpoint = "/api/v3/repos/acme/metrics/traffic/views";
    checked(
        &router,
        "repos/get-views",
        "GET",
        org_endpoint,
        Some(&bob_token),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[carol])
        .await;
    checked(
        &router,
        "repos/get-views",
        "GET",
        org_endpoint,
        Some(&outsider),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[alice])
        .await;
    checked(
        &router,
        "repos/get-views",
        "GET",
        org_endpoint,
        Some(&bob_token),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
    let (status, body) = navigation(
        &router,
        None,
        "/acme/metrics/issues",
        "GET",
        Mode::Html,
        address,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    f.execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[alice])
        .await;
    let (_, data) = checked(
        &router,
        "repos/get-views",
        "GET",
        org_endpoint,
        Some(&bob_token),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(data["count"], 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_github_repository_traffic() {
    traffic_contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_github_repository_traffic() {
    traffic_contract(true).await;
}
