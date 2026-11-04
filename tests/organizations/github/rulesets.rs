use super::request;
use crate::support::{database::Database, http_client::Client, organizations::applied};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use codeyard::backend::db::organizations;
use codeyard::backend::db::organizations::NewOrganization;
use codeyard::backend::db::organizations::OrganizationProfile;
use codeyard::backend::identity::tokens;
use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};
use tower::ServiceExt;

const LIST: &str = "repos/get-org-rulesets";
const CREATE: &str = "repos/create-org-ruleset";
const GET: &str = "repos/get-org-ruleset";
const UPDATE: &str = "repos/update-org-ruleset";
const DELETE: &str = "repos/delete-org-ruleset";
const HISTORY: &str = "orgs/get-org-ruleset-history";
const VERSION: &str = "orgs/get-org-ruleset-version";
const SUITES: &str = "repos/get-org-rule-suites";
const SUITE: &str = "repos/get-org-rule-suite";
const PRODUCT: &str = "orgs/enable-or-disable-security-product-on-all-org-repos";

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let member = f.actor("member").await;
    let outside = f.actor("outside").await;
    let org = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let foreign = applied(
        organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("foreign", OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        organizations::add_organization_member(
            f.pool(),
            owner,
            org.id,
            member,
            codeyard_db_core::models::OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (owner, "read"),
        (member, "write"),
        (outside, "write"),
    ] {
        let browser = Client::login(&router, f.pool(), user).await;
        let token = tokens::create(
            f.pool(),
            user,
            Some(&browser.session),
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
    let read = Some(credentials[1].as_str());
    let member_auth = Some(credentials[2].as_str());
    let outside_auth = Some(credentials[3].as_str());
    let path = "/api/v3/orgs/acme/rulesets";
    let input = json!({"name":"Release policy","enforcement":"disabled","conditions":{"repository_name":{"include":["~ALL"]},"ref_name":{"include":["~DEFAULT_BRANCH"]}},"bypass_actors":[{"actor_type":"OrganizationAdmin"}],"rules":[{"type":"non_fast_forward"}]});
    request(&router, LIST, "GET", path, None, Value::Null, 401).await;
    request(&router, LIST, "GET", path, outside_auth, Value::Null, 403).await;
    for token in [read, member_auth, outside_auth] {
        request(&router, CREATE, "POST", path, token, input.clone(), 403).await;
    }
    let (_, rule) = request(&router, CREATE, "POST", path, write, input.clone(), 201).await;
    assert_eq!(rule["source"], "acme");
    assert_eq!(rule["source_type"], "Organization");
    assert_eq!(rule["bypass_actors"][0]["actor_type"], "OrganizationAdmin");
    let id = rule["id"].as_i64().unwrap();
    let one = format!("{path}/{id}");
    assert!(rule["_links"]["self"]["href"]
        .as_str()
        .unwrap()
        .ends_with(&one));
    let (_, visible) = request(&router, GET, "GET", &one, member_auth, Value::Null, 200).await;
    assert!(visible.get("bypass_actors").is_none());
    assert_eq!(visible["rules"], input["rules"]);
    request(
        &router,
        GET,
        "GET",
        &format!("/api/v3/orgs/foreign/rulesets/{id}"),
        write,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        GET,
        "GET",
        &format!("{path}/99999"),
        write,
        Value::Null,
        404,
    )
    .await;
    // Evaluate mode is accepted and recorded as rule suites; active rules are not enforced yet.
    request(
        &router,
        UPDATE,
        "PUT",
        &one,
        write,
        json!({"enforcement":"active"}),
        422,
    )
    .await;
    let mut active = input.clone();
    active["enforcement"] = json!("active");
    request(&router, CREATE, "POST", path, write, active, 422).await;
    for input in [
        json!({"target":"repository"}),
        json!({"rules":[{"type":"unknown"}]}),
        json!({"bypass_actors":[{"actor_type":"User","actor_id":-1}]}),
        json!({"conditions":{"repository_id":{"repository_ids":[999999]},"ref_name":{"include":["~ALL"]}}}),
        json!({"name":"bad\nname"}),
    ] {
        request(&router, UPDATE, "PUT", &one, write, input, 422).await;
    }
    for token in [read, member_auth, outside_auth] {
        request(
            &router,
            UPDATE,
            "PUT",
            &one,
            token,
            json!({"name":"Denied"}),
            403,
        )
        .await;
        request(&router, DELETE, "DELETE", &one, token, Value::Null, 403).await;
    }
    let (_, renamed) = request(
        &router,
        UPDATE,
        "PUT",
        &one,
        write,
        json!({"name":"Renamed"}),
        200,
    )
    .await;
    assert_eq!(renamed["rules"], rule["rules"]);
    assert_eq!(renamed["conditions"], rule["conditions"]);
    assert_ne!(renamed["updated_at"], rule["updated_at"]);
    request(&router, UPDATE, "PUT", &one, write, json!({}), 200).await;
    let empty_update = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(&one)
                .header("authorization", credentials[0].as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty_update.status(), StatusCode::OK);
    let history = format!("{one}/history");
    let (headers, versions) = request(
        &router,
        HISTORY,
        "GET",
        &format!("{history}?per_page=1"),
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(versions.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert_eq!(versions[0]["actor"]["id"], owner);
    let (_, old) = request(
        &router,
        HISTORY,
        "GET",
        &format!("{history}?page=2&per_page=1"),
        write,
        Value::Null,
        200,
    )
    .await;
    let version_id = old[0]["version_id"].as_i64().unwrap();
    let (_, snapshot) = request(
        &router,
        VERSION,
        "GET",
        &format!("{history}/{version_id}"),
        member_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(snapshot["state"]["name"], "Release policy");
    assert!(snapshot["state"].get("bypass_actors").is_none());
    request(
        &router,
        VERSION,
        "GET",
        &format!("{history}/99999"),
        write,
        Value::Null,
        404,
    )
    .await;
    let (_, all_versions) =
        request(&router, HISTORY, "GET", &history, read, Value::Null, 200).await;
    assert_eq!(
        all_versions.as_array().unwrap().len(),
        2,
        "failed and no-op updates must not create versions"
    );
    let (_,tag) = request(&router,CREATE,"POST",path,write,json!({"name":"Tag policy","target":"tag","enforcement":"disabled","rules":[{"type":"deletion"}]}),201).await;
    let (headers, first) = request(
        &router,
        LIST,
        "GET",
        &format!("{path}?per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(first[0]["id"], id);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, filtered) = request(
        &router,
        LIST,
        "GET",
        &format!("{path}?targets=tag&per_page=1"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(filtered[0]["id"], tag["id"]);
    let (_, empty) = request(
        &router,
        LIST,
        "GET",
        &format!("{path}?targets=push"),
        read,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(empty, json!([]));
    request(
        &router,
        LIST,
        "GET",
        &format!("{path}?targets=unknown"),
        read,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        LIST,
        "GET",
        &format!("{path}?page=0"),
        read,
        Value::Null,
        400,
    )
    .await;
    let (_, repo) = request(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        write,
        json!({"name":"private","private":true}),
        201,
    )
    .await;
    let repo_id = repo["id"].as_i64().unwrap();
    let before = "a".repeat(40);
    let after = "b".repeat(40);
    for (reference, result, enforcement) in [
        ("refs/heads/main", "fail", "active"),
        ("refs/tags/main", "pass", "evaluate"),
        ("refs/heads/other", "bypass", "active"),
    ] {
        let evaluations = json!([{"rule_source":{"id":id,"name":"Renamed","type":"Organization"},"enforcement":enforcement,"result":"fail","rule_type":"non_fast_forward","details":"Refused","metadata":null}]);
        f.execute(&format!("INSERT INTO repository_rule_suites(org_id,repo_id,repository_name,actor_id,actor_name,ref_name,before_sha,after_sha,result,evaluations) VALUES($1,$2,'private',$3,'owner','{reference}','{before}','{after}','{result}','{evaluations}')"),&[org.id,repo_id,owner]).await;
    }
    let suites = format!("{path}/rule-suites");
    let (headers, listed) = request(
        &router,
        SUITES,
        "GET",
        &format!("{suites}?per_page=1"),
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed[0]["result"], "bypass");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let suite_id = listed[0]["id"].as_i64().unwrap();
    for (query, count) in [
        ("ref=main", 2),
        ("ref=refs%2Fheads%2Fmain", 1),
        ("ref=main&rule_suite_result=pass", 1),
        ("repository_name=PRIVATE&actor_name=OWNER", 3),
        ("repository_name=missing", 0),
        ("actor_name=member", 0),
        ("evaluate_status=evaluate", 1),
        ("evaluate_status=active", 2),
        ("rule_suite_result=fail", 1),
        ("rule_suite_result=all&time_period=hour", 3),
    ] {
        let (_, rows) = request(
            &router,
            SUITES,
            "GET",
            &format!("{suites}?{query}"),
            write,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(rows.as_array().unwrap().len(), count, "{query}");
    }
    for query in [
        "ref=*",
        "time_period=year",
        "evaluate_status=invalid",
        "rule_suite_result=invalid",
    ] {
        request(
            &router,
            SUITES,
            "GET",
            &format!("{suites}?{query}"),
            write,
            Value::Null,
            422,
        )
        .await;
    }
    let (_, hidden) = request(
        &router,
        SUITES,
        "GET",
        &suites,
        member_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(hidden, json!([]));
    request(
        &router,
        SUITE,
        "GET",
        &format!("{suites}/{suite_id}"),
        member_auth,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        SUITE,
        "GET",
        &format!("/api/v3/orgs/foreign/rulesets/rule-suites/{suite_id}"),
        write,
        Value::Null,
        404,
    )
    .await;
    let (_, detail) = request(
        &router,
        SUITE,
        "GET",
        &format!("{suites}/{suite_id}"),
        write,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(detail["repository_id"], repo_id);
    assert_eq!(detail["evaluation_result"], Value::Null);
    assert!(detail["rule_evaluations"][0].get("metadata").is_none());
    let (_, detail) = request(
        &router,
        SUITE,
        "GET",
        &format!("{suites}/{suite_id}?include_metadata=true"),
        write,
        Value::Null,
        200,
    )
    .await;
    assert!(detail["rule_evaluations"][0].get("metadata").is_some());
    f.execute(
        "UPDATE repository_rule_suites SET created_at='2000-01-01T00:00:00Z' WHERE id=$1",
        &[suite_id],
    )
    .await;
    let (_, recent) = request(&router, SUITES, "GET", &suites, write, Value::Null, 200).await;
    assert_eq!(recent.as_array().unwrap().len(), 2);
    f.execute(
        "UPDATE repositories SET owner_org_id=$1 WHERE id=$2",
        &[foreign.id, repo_id],
    )
    .await;
    let (_, transferred) = request(&router, SUITES, "GET", &suites, write, Value::Null, 200).await;
    assert_eq!(transferred, json!([]));
    request(
        &router,
        SUITE,
        "GET",
        &format!("{suites}/{suite_id}"),
        write,
        Value::Null,
        404,
    )
    .await;
    for product in [
        "dependency_graph",
        "dependabot_alerts",
        "dependabot_security_updates",
        "advanced_security",
        "code_scanning_default_setup",
        "secret_scanning",
        "secret_scanning_push_protection",
    ] {
        for action in ["enable_all", "disable_all"] {
            request(
                &router,
                PRODUCT,
                "POST",
                &format!("/api/v3/orgs/acme/{product}/{action}"),
                write,
                Value::Null,
                422,
            )
            .await;
        }
    }
    for (token, status) in [
        (None, 401),
        (read, 403),
        (member_auth, 403),
        (outside_auth, 403),
    ] {
        request(
            &router,
            PRODUCT,
            "POST",
            "/api/v3/orgs/acme/secret_scanning/enable_all",
            token,
            Value::Null,
            status,
        )
        .await;
    }
    request(&router, DELETE, "DELETE", &one, write, Value::Null, 204).await;
    request(&router, HISTORY, "GET", &history, write, Value::Null, 404).await;
    request(&router, GET, "GET", &one, write, Value::Null, 404).await;
    request(&router, DELETE, "DELETE", &one, write, Value::Null, 404).await;
    f.finish().await;
}
#[tokio::test]
async fn github_organization_rulesets_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_organization_rulesets_postgres() {
    contract(true).await;
}
