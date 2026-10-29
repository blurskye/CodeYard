use super::github::{document, request};
use crate::support::{database::Database, http_client::Client};
use axum::http::StatusCode;
use codeyard::backend::{
    db,
    identity::tokens::{self, TokenInput},
};
use serde_json::{json, Value};
fn url(kind: &str, q: &str, extra: &[(&str, String)]) -> String {
    let mut pairs = url::form_urlencoded::Serializer::new(String::new());
    pairs.append_pair("q", q);
    for (k, v) in extra {
        pairs.append_pair(k, v);
    }
    format!("/api/v3/search/{kind}?{}", pairs.finish())
}
async fn search(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("needleuser").await;
    f.execute(
        "UPDATE users SET email='private-alice@example.test',email_verified=true WHERE id=$1",
        &[alice],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut repositories = Vec::new();
    for (name, private, desc) in [
        ("proj", false, "needle analysis"),
        ("other", false, "needle"),
        ("secret", true, "hidden needle analysis"),
    ] {
        repositories.push(
            codeyard::backend::repositories::lifecycle::provision(
                (f.pool(), None),
                root.path(),
                db::repositories::creation::RepositoryCreationOwner::personal(alice),
                name,
                Some(desc),
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let repo = &repositories[0];
    let private = &repositories[2];
    let org = match db::organizations::create_organization(
        f.pool(),
        alice,
        &db::organizations::NewOrganization::new(
            "needlegroup",
            db::organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org,
        _ => panic!("organization"),
    };
    let router = f.router(root.path()).await;
    let browser = Client::login(&router, f.pool(), alice).await;
    let outsider = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (id, scope, session) in [
        (alice, "write", browser.session.as_str()),
        (alice, "read", browser.session.as_str()),
        (bob, "write", outsider.session.as_str()),
    ] {
        let token = tokens::create(
            f.pool(),
            id,
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
    let (write, read, other) = (&credentials[0], &credentials[1], &credentials[2]);
    assert_eq!(
        request(
            &router,
            "PUT",
            "/api/v3/user/starred/alice/other",
            Some(other),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let query = url(
        "repositories",
        "needle",
        &[("per_page", "1".into()), ("sort", "stars".into())],
    );
    let first = request(&router, "GET", &query, None, Value::Null).await;
    assert!(first.headers()["link"].to_str().unwrap().contains("page=2"));
    let first = document(first, "search/repos", 200).await;
    assert_eq!(first["total_count"], 2);
    assert_eq!(first["items"][0]["name"], "other");
    assert_eq!(first["incomplete_results"], false);
    let owner = document(
        request(
            &router,
            "GET",
            &url("repositories", "needle", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/repos",
        200,
    )
    .await;
    assert_eq!(owner["total_count"], 3);
    let mine = document(
        request(
            &router,
            "GET",
            &url("repositories", "user:@me", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/repos",
        200,
    )
    .await;
    assert_eq!(mine["total_count"], 3);
    assert_eq!(
        request(
            &router,
            "GET",
            &url("repositories", "user:@me", &[]),
            None,
            Value::Null
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(owner["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["private"] == true));
    let anonymous = document(
        request(
            &router,
            "GET",
            &url("repositories", "repo:alice/secret", &[]),
            None,
            Value::Null,
        )
        .await,
        "search/repos",
        200,
    )
    .await;
    assert_eq!(anonymous["total_count"], 0);
    for (query, total) in [
        ("\"needle analysis\"", 1),
        ("needle -repo:alice/other", 1),
        ("repo:alice/proj OR repo:alice/other", 2),
        ("user:alice stars:>=1", 1),
        ("needle created:>=2000-01-01", 2),
        (".*", 0),
    ] {
        let value = document(
            request(
                &router,
                "GET",
                &url("repositories", query, &[]),
                None,
                Value::Null,
            )
            .await,
            "search/repos",
            200,
        )
        .await;
        assert_eq!(value["total_count"], total, "{query}: {value}");
    }
    for (name, topics) in [
        ("proj", vec!["rust", "public-only"]),
        ("secret", vec!["rust", "private-only"]),
        ("other", vec!["python"]),
    ] {
        document(
            request(
                &router,
                "PUT",
                &format!("/api/v3/repos/alice/{name}/topics"),
                Some(write),
                json!({"names":topics}),
            )
            .await,
            "repos/replace-all-topics",
            200,
        )
        .await;
    }
    // Each endpoint conversation gets a fresh budget in this disposable database.
    f.execute("DELETE FROM github_api_limits WHERE resource='search'", &[])
        .await;
    let topics = document(
        request(
            &router,
            "GET",
            &url("topics", "rust", &[]),
            None,
            Value::Null,
        )
        .await,
        "search/topics",
        200,
    )
    .await;
    assert_eq!(topics["total_count"], 1);
    assert_eq!(topics["items"][0]["repository_count"], 1);
    assert_eq!(topics["items"][0]["curated"], false);
    let owned = document(
        request(
            &router,
            "GET",
            &url("topics", "rust", &[]),
            Some(read),
            Value::Null,
        )
        .await,
        "search/topics",
        200,
    )
    .await;
    assert_eq!(owned["items"][0]["repository_count"], 2);
    for (token, total) in [(None, 0), (Some(read), 1), (Some(other), 0)] {
        let value = document(
            request(
                &router,
                "GET",
                &url("topics", "private-only", &[]),
                token.map(String::as_str),
                Value::Null,
            )
            .await,
            "search/topics",
            200,
        )
        .await;
        assert_eq!(value["total_count"], total);
    }
    let found = document(
        request(
            &router,
            "GET",
            &url("repositories", "topic:rust user:alice", &[]),
            None,
            Value::Null,
        )
        .await,
        "search/repos",
        200,
    )
    .await;
    assert_eq!(found["total_count"], 1);
    f.execute("DELETE FROM github_api_limits WHERE resource='search'", &[])
        .await;
    let users = document(
        request(
            &router,
            "GET",
            &url("users", "needle type:user", &[]),
            None,
            Value::Null,
        )
        .await,
        "search/users",
        200,
    )
    .await;
    assert_eq!(users["total_count"], 1);
    assert_eq!(users["items"][0]["id"], bob);
    assert_eq!(users["items"][0]["type"], "User");
    let groups = document(
        request(
            &router,
            "GET",
            &url("users", "needle type:org", &[]),
            None,
            Value::Null,
        )
        .await,
        "search/users",
        200,
    )
    .await;
    assert_eq!(groups["total_count"], 1);
    assert_eq!(groups["items"][0]["id"], org.id);
    assert_eq!(groups["items"][0]["type"], "Organization");
    let email = db::users::accounts::get_user_by_id(f.pool(), alice)
        .await
        .unwrap()
        .unwrap()
        .email
        .unwrap();
    let private_email = document(
        request(
            &router,
            "GET",
            &url("users", &format!("{email} in:email"), &[]),
            None,
            Value::Null,
        )
        .await,
        "search/users",
        200,
    )
    .await;
    assert_eq!(private_email["total_count"], 0);
    assert_eq!(
        request(
            &router,
            "PUT",
            "/api/v3/user/following/needleuser",
            Some(write),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let followed = document(
        request(
            &router,
            "GET",
            &url(
                "users",
                "type:user followers:>=1",
                &[("sort", "followers".into())],
            ),
            None,
            Value::Null,
        )
        .await,
        "search/users",
        200,
    )
    .await;
    assert_eq!(followed["total_count"], 1);
    assert_eq!(followed["items"][0]["id"], bob);
    let hidden_count = document(
        request(
            &router,
            "GET",
            &url("users", "alice repos:>=3", &[]),
            None,
            Value::Null,
        )
        .await,
        "search/users",
        200,
    )
    .await;
    assert_eq!(hidden_count["total_count"], 0);
    let mut label_id = 0;
    for (name, repo_name) in [
        ("bug", "proj"),
        ("bug%literal", "proj"),
        ("道路", "proj"),
        ("private bug", "secret"),
    ] {
        let label = document(
            request(
                &router,
                "POST",
                &format!("/api/v3/repos/alice/{repo_name}/labels"),
                Some(write),
                json!({"name":name,"color":"123abc","description":"A label"}),
            )
            .await,
            "issues/create-label",
            201,
        )
        .await;
        if name == "bug" {
            label_id = label["id"].as_i64().unwrap();
        }
    }
    f.execute("DELETE FROM github_api_limits WHERE resource='search'", &[])
        .await;
    let scope = vec![("repository_id", repo.id.to_string())];
    let labels = document(
        request(
            &router,
            "GET",
            &url("labels", "%", &scope),
            None,
            Value::Null,
        )
        .await,
        "search/labels",
        200,
    )
    .await;
    assert_eq!(labels["total_count"], 1);
    assert_eq!(labels["items"][0]["name"], "bug%literal");
    document(
        request(
            &router,
            "PATCH",
            "/api/v3/repos/alice/proj/labels/bug",
            Some(write),
            json!({"new_name":"bug updated"}),
        )
        .await,
        "issues/update-label",
        200,
    )
    .await;
    let mut updated = scope.clone();
    updated.push(("sort", "updated".into()));
    let labels = document(
        request(
            &router,
            "GET",
            &url("labels", "bug OR 道路", &updated),
            None,
            Value::Null,
        )
        .await,
        "search/labels",
        200,
    )
    .await;
    assert_eq!(labels["total_count"], 3);
    assert_eq!(labels["items"][0]["id"], label_id);
    let private_scope = vec![("repository_id", private.id.to_string())];
    document(
        request(
            &router,
            "GET",
            &url("labels", "bug", &private_scope),
            None,
            Value::Null,
        )
        .await,
        "search/labels",
        404,
    )
    .await;
    let labels = document(
        request(
            &router,
            "GET",
            &url("labels", "bug", &private_scope),
            Some(read),
            Value::Null,
        )
        .await,
        "search/labels",
        200,
    )
    .await;
    assert_eq!(labels["total_count"], 1);
    for (kind, op) in [
        ("repositories", "search/repos"),
        ("users", "search/users"),
        ("topics", "search/topics"),
    ] {
        f.execute("DELETE FROM github_api_limits WHERE resource='search'", &[])
            .await;
        for q in ["", "\"unterminated", "unknown:term", "created:not-a-date"] {
            assert_eq!(
                request(&router, "GET", &url(kind, q, &[]), None, Value::Null)
                    .await
                    .status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{kind} {q}"
            );
        }
        document(
            request(
                &router,
                "GET",
                &url(kind, "needle", &[("page", "1001".into())]),
                None,
                Value::Null,
            )
            .await,
            op,
            422,
        )
        .await;
    }
    let explore = browser.page_ok("/explore?topic=rust").await;
    assert!(explore.contains("rust"));
    f.finish().await;
}
#[tokio::test]
async fn github_catalog_search_sqlite() {
    search(false).await;
}
#[tokio::test]
async fn github_catalog_search_postgres() {
    search(true).await;
}
