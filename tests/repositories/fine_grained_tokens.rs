//! Fine-grained personal access tokens on repositories: each repository selection, read and
//! write grants, and the resource owner, through REST, GraphQL and Git over HTTP on both
//! engines.
use crate::support::{database::Database, http_client::Client, organizations::applied};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{Duration, Utc};
use codeyard::backend::{
    db::{
        self,
        organizations::{NewOrganization, OrganizationProfile},
        repositories::creation::{RepositoryCreationOwner, RepositoryTarget},
    },
    identity::fine_grained::{self, Access, Owner},
    repositories::lifecycle,
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (u16, HeaderMap, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

async fn graphql(router: &Router, token: &str, query: &str, variables: Value) -> Value {
    let (status, _, body) = call(
        router,
        "POST",
        "/api/graphql",
        token,
        json!({"query": query, "variables": variables}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body
}

async fn git_refs(router: &Router, token: &str, repo: &str, service: &str) -> u16 {
    router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{repo}.git/info/refs?service={service}"))
                .header(
                    "authorization",
                    format!(
                        "Basic {}",
                        STANDARD.encode(format!("x-access-token:{token}"))
                    ),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn token(
    pool: &db::DbPool,
    user: i64,
    session: &str,
    owner: Owner,
    access: Access,
    repositories: Vec<i64>,
    permissions: Value,
) -> String {
    fine_grained::create(
        pool,
        user,
        Some(session),
        fine_grained::Request {
            name: "test".into(),
            description: String::new(),
            owner,
            access,
            repositories,
            permissions: permissions.as_object().unwrap().clone(),
            expires_at: Some(Utc::now() + Duration::days(30)),
        },
    )
    .await
    .unwrap()
    .value
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let org = applied(
        db::organizations::create_organization(
            f.pool(),
            alice,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let root = tempfile::tempdir().unwrap();
    let mut repos = Vec::new();
    for (target, name, private) in [
        (RepositoryTarget::Personal(alice), "private", true),
        (RepositoryTarget::Personal(alice), "other", true),
        (RepositoryTarget::Personal(alice), "open", false),
        (RepositoryTarget::Organization(org), "internal", true),
    ] {
        repos.push(
            lifecycle::provision(
                (f.pool(), None),
                root.path(),
                RepositoryCreationOwner {
                    actor_id: alice,
                    target,
                },
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap(),
        );
    }
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await.session;
    let user = Owner::User(alice);
    let public = token(
        f.pool(),
        alice,
        &session,
        user,
        Access::Public,
        vec![],
        json!({}),
    )
    .await;
    let all_read = token(
        f.pool(),
        alice,
        &session,
        user,
        Access::All,
        vec![],
        json!({"issues":"read","contents":"read"}),
    )
    .await;
    let selected = token(
        f.pool(),
        alice,
        &session,
        user,
        Access::Selected,
        vec![repos[0].id],
        json!({"issues":"write","contents":"write"}),
    )
    .await;
    let org_all = token(
        f.pool(),
        alice,
        &session,
        Owner::Organization(org),
        Access::All,
        vec![],
        json!({"issues":"read"}),
    )
    .await;
    assert!(public.starts_with("cy_fgpat_") && public.len() == 73);

    // REST: what each selection reaches. Outside the token a private repository is missing.
    for (token, path, expected) in [
        (&public, "/api/v3/repos/alice/private", 404),
        (&public, "/api/v3/repos/alice/open", 200),
        (&public, "/api/v3/repos/alice/open/issues", 200),
        (&all_read, "/api/v3/repos/alice/private", 200),
        (&all_read, "/api/v3/repos/alice/other/issues", 200),
        (&all_read, "/api/v3/repos/acme/internal", 404),
        (&selected, "/api/v3/repos/alice/private/issues", 200),
        (&selected, "/api/v3/repos/alice/other", 404),
        (&org_all, "/api/v3/repos/acme/internal/issues", 200),
        (&org_all, "/api/v3/repos/alice/private", 404),
    ] {
        let (status, headers, body) = call(&router, "GET", path, token, Value::Null).await;
        assert_eq!(status, expected, "{path}: {body}");
        assert!(
            headers.get("x-oauth-scopes").is_none(),
            "fine-grained tokens have no scopes"
        );
    }
    // A read grant cannot write, and a write grant writes only where it reaches.
    let issue = json!({"title": "From a fine-grained token"});
    for (token, repo, expected) in [
        (&all_read, "alice/private", 403),
        (&public, "alice/open", 403),
        (&selected, "alice/open", 403),
        (&org_all, "acme/internal", 403),
        (&selected, "alice/private", 201),
    ] {
        let path = format!("/api/v3/repos/{repo}/issues");
        let (status, headers, body) = call(&router, "POST", &path, token, issue.clone()).await;
        assert_eq!(status, expected, "{path}: {body}");
        assert_eq!(headers["x-accepted-github-permissions"], "issues=write");
    }
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM issues", &[]).await,
        1,
        "only the permitted write created an issue"
    );
    // Account and organization APIs need their own permissions.
    for (token, method, path, expected) in [
        (&all_read, "GET", "/api/v3/user", 200),
        (&all_read, "GET", "/api/v3/user/keys", 403),
        (&all_read, "GET", "/api/v3/notifications", 403),
        (&org_all, "GET", "/api/v3/orgs/acme/hooks", 403),
    ] {
        let (status, _, body) = call(&router, method, path, token, Value::Null).await;
        assert_eq!(status, expected, "{path}: {body}");
    }

    // GraphQL: repositories through the same bounds, and no cross-repository fields.
    let name = "query($name:String!){ repository(owner:\"alice\",name:$name){ id name } }";
    let reached = graphql(&router, &selected, name, json!({"name":"private"})).await;
    assert_eq!(
        reached["data"]["repository"]["name"], "private",
        "{reached}"
    );
    let hidden = graphql(&router, &selected, name, json!({"name":"other"})).await;
    assert!(hidden["data"]["repository"].is_null(), "{hidden}");
    assert!(hidden["errors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty()));
    let viewer = graphql(&router, &public, "{ viewer { login } }", json!({})).await;
    assert_eq!(viewer["data"]["viewer"]["login"], "alice", "{viewer}");
    // Only an explicit contents grant may include unpublished releases. A selected public
    // repository keeps public read access, as on GitHub, so a metadata-only token still
    // reads its published releases.
    db::releases::create_release(
        f.pool(),
        alice,
        repos[2].id,
        "unpublished",
        "main",
        &db::releases::ReleaseFields {
            is_draft: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let releases = "{ repository(owner:\"alice\",name:\"open\"){ releases(first:0){totalCount} } }";
    let metadata = token(
        f.pool(),
        alice,
        &session,
        user,
        Access::Selected,
        vec![repos[2].id],
        json!({}),
    )
    .await;
    for (credential, count) in [(&metadata, 0), (&public, 0), (&selected, 0), (&all_read, 1)] {
        let result = graphql(&router, credential, releases, json!({})).await;
        assert!(
            result["errors"]
                .as_array()
                .is_none_or(|errors| errors.is_empty()),
            "{result}"
        );
        assert_eq!(
            result["data"]["repository"]["releases"]["totalCount"], count,
            "{result}"
        );
    }
    let id = reached["data"]["repository"]["id"].clone();
    let create = "mutation($id:ID!){ createIssue(input:{repositoryId:$id,title:\"From GraphQL\"}){ issue { title } } }";
    let denied = graphql(&router, &all_read, create, json!({"id": id})).await;
    assert!(
        denied["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "{denied}"
    );
    let created = graphql(&router, &selected, create, json!({"id": id})).await;
    assert_eq!(
        created["data"]["createIssue"]["issue"]["title"], "From GraphQL",
        "{created}"
    );
    let search = graphql(
        &router,
        &all_read,
        "{ search(query:\"proj\",type:REPOSITORY,first:1){ repositoryCount } }",
        json!({}),
    )
    .await;
    assert!(
        search["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "{search}"
    );

    // Git over HTTP: contents read clones, contents write pushes, the rest is missing.
    for (token, repo, service, expected) in [
        (&all_read, "alice/private", "git-upload-pack", 200),
        (&all_read, "alice/private", "git-receive-pack", 403),
        (&selected, "alice/private", "git-receive-pack", 200),
        (&selected, "alice/other", "git-upload-pack", 404),
        (&public, "alice/private", "git-upload-pack", 404),
        (&public, "alice/open", "git-upload-pack", 200),
        (&org_all, "acme/internal", "git-upload-pack", 403),
    ] {
        assert_eq!(
            git_refs(&router, token, repo, service).await,
            expected,
            "{repo} {service}"
        );
    }
    // Merges carry the token to publication: without the workflows permission a merge may
    // leave the workflow files alone but not change them, even through a detached commit.
    let base = "/api/v3/repos/alice/private";
    let (status, _, seed) = call(
        &router,
        "PUT",
        &format!("{base}/contents/README.md"),
        &selected,
        json!({"message":"Seed","content":"c2VlZA=="}),
    )
    .await;
    assert_eq!(status, 201, "{seed}");
    let main = seed["commit"]["sha"].clone();
    let main_tree = seed["commit"]["tree"]["sha"].clone();
    let mut heads = Vec::new();
    for path in [".github/workflows/ci.yml", "docs/notes.md"] {
        let (status, _, tree) = call(
            &router,
            "POST",
            &format!("{base}/git/trees"),
            &selected,
            json!({"base_tree":main_tree,"tree":[{"path":path,"mode":"100644","type":"blob","content":"on: push\n"}]}),
        )
        .await;
        assert_eq!(status, 201, "{tree}");
        let (status, _, commit) = call(
            &router,
            "POST",
            &format!("{base}/git/commits"),
            &selected,
            json!({"message":"Change","tree":tree["sha"],"parents":[main]}),
        )
        .await;
        assert_eq!(status, 201, "{commit}");
        heads.push(commit["sha"].clone());
    }
    for (head, expected) in [(&heads[0], 403), (&heads[1], 201)] {
        let (status, _, body) = call(
            &router,
            "POST",
            &format!("{base}/merges"),
            &selected,
            json!({"base":"main","head":head}),
        )
        .await;
        assert_eq!(status, expected, "{body}");
    }
    // Every use is recorded on the token, as for classic tokens.
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM personal_access_tokens WHERE user_id=$1 AND last_used_at IS NULL",
            &[alice]
        )
        .await,
        0
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite() {
    contract(false).await;
}

#[tokio::test]
async fn postgres() {
    contract(true).await;
}
