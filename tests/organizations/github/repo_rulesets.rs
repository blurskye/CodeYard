use super::request;
use crate::support::{
    codeyard::backend::db::organizations::applied, database::Database, http_client::Client,
};
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};

use codeyard::backend::db::organizations::NewOrganization;
use codeyard::backend::db::organizations::OrganizationProfile;

use codeyard::backend::identity::tokens::TokenInput;
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};
use tower::ServiceExt;

const LIST: &str = "repos/get-repo-rulesets";
const CREATE: &str = "repos/create-repo-ruleset";
const GET: &str = "repos/get-repo-ruleset";
const UPDATE: &str = "repos/update-repo-ruleset";
const DELETE: &str = "repos/delete-repo-ruleset";
const HISTORY: &str = "repos/get-repo-ruleset-history";
const VERSION: &str = "repos/get-repo-ruleset-version";

/// Sends one of gh's ruleset list documents and returns `data.level.rulesets`.
async fn graphql(router: &Router, token: &str, query: &str, variables: Value) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::post("/api/graphql")
                .header("authorization", token)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"query":query,"variables":variables}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    body["data"]["level"]["rulesets"].clone()
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("owner").await;
    let admin = f.actor("admin").await;
    let writer = f.actor("writer").await;
    let reader = f.actor("reader").await;
    let org = applied(
        codeyard::backend::db::organizations::create_organization(
            f.pool(),
            owner,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard::backend::db::organizations::add_organization_member(
            f.pool(),
            owner,
            org.id,
            admin,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (user, scope) in [
        (owner, "write"),
        (admin, "write"),
        (writer, "write"),
        (reader, "read"),
        (admin, "read"),
    ] {
        let browser = Client::login(&router, f.pool(), user).await;
        let token = codeyard::backend::identity::tokens::create(
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
    let owner_auth = Some(credentials[0].as_str());
    let admin_auth = Some(credentials[1].as_str());
    let writer_auth = Some(credentials[2].as_str());
    let reader_auth = Some(credentials[3].as_str());
    let read_admin = Some(credentials[4].as_str());
    let (_, repo) = request(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        owner_auth,
        json!({"name":"private","private":true}),
        201,
    )
    .await;
    let repo_id = repo["id"].as_i64().unwrap();
    for (user, level) in [(admin, 5), (writer, 3), (reader, 1)] {
        f.execute(
            "INSERT INTO repo_access(repo_id,user_id,access_level) VALUES($1,$2,$3)",
            &[repo_id, user, level],
        )
        .await;
    }
    request(
        &router,
        "repos/create-in-org",
        "POST",
        "/api/v3/orgs/acme/repos",
        owner_auth,
        json!({"name":"other"}),
        201,
    )
    .await;
    request(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        owner_auth,
        json!({"name":"personal"}),
        201,
    )
    .await;
    let path = "/api/v3/repos/acme/private/rulesets";
    let input = json!({"name":"Release policy","enforcement":"disabled","conditions":{"ref_name":{"include":["~DEFAULT_BRANCH"],"exclude":[]}},"bypass_actors":[{"actor_type":"RepositoryRole","actor_id":5}],"rules":[{"type":"non_fast_forward"}]});
    request(&router, LIST, "GET", path, None, Value::Null, 404).await;
    request(&router, CREATE, "POST", path, None, input.clone(), 401).await;
    for auth in [writer_auth, reader_auth, read_admin] {
        request(&router, CREATE, "POST", path, auth, input.clone(), 403).await;
    }
    let (_, rule) = request(
        &router,
        CREATE,
        "POST",
        path,
        admin_auth,
        input.clone(),
        201,
    )
    .await;
    assert_eq!(rule["source"], "acme/private");
    assert_eq!(rule["source_type"], "Repository");
    assert_eq!(rule["conditions"], input["conditions"]);
    assert_eq!(rule["bypass_actors"][0]["actor_type"], "RepositoryRole");
    let id = rule["id"].as_i64().unwrap();
    let one = format!("{path}/{id}");
    assert!(rule["_links"]["self"]["href"]
        .as_str()
        .unwrap()
        .ends_with(&one));
    for auth in [writer_auth, reader_auth] {
        let (_, visible) = request(&router, GET, "GET", &one, auth, Value::Null, 200).await;
        assert!(visible.get("bypass_actors").is_none());
        assert_eq!(visible["rules"], input["rules"]);
    }
    for suffix in ["", "/history", "/history/1"] {
        let op = match suffix {
            "" => GET,
            "/history" => HISTORY,
            _ => VERSION,
        };
        request(
            &router,
            op,
            "GET",
            &format!("/api/v3/repos/acme/other/rulesets/{id}{suffix}"),
            owner_auth,
            Value::Null,
            404,
        )
        .await;
        request(
            &router,
            op,
            "GET",
            &format!("/api/v3/orgs/acme/rulesets/{id}{suffix}"),
            owner_auth,
            Value::Null,
            404,
        )
        .await;
    }
    // Evaluate mode is accepted and recorded as rule suites; active rules are not enforced yet.
    request(
        &router,
        UPDATE,
        "PUT",
        &one,
        admin_auth,
        json!({"enforcement":"active"}),
        422,
    )
    .await;
    let mut active = input.clone();
    active["enforcement"] = json!("active");
    request(&router, CREATE, "POST", path, admin_auth, active, 422).await;
    for invalid in [
        json!({"target":"repository"}),
        json!({"conditions":{"repository_name":{"include":["~ALL"]},"ref_name":{"include":["~ALL"]}}}),
        json!({"conditions":{"ref_name":{"include":["~UNKNOWN"]}}}),
        json!({"rules":[{"type":"unknown"}]}),
        json!({"bypass_actors":[{"actor_type":"Team","actor_id":999999}]}),
    ] {
        request(&router, UPDATE, "PUT", &one, admin_auth, invalid, 422).await;
    }
    for auth in [writer_auth, reader_auth, read_admin] {
        request(
            &router,
            UPDATE,
            "PUT",
            &one,
            auth,
            json!({"name":"Denied"}),
            403,
        )
        .await;
        request(&router, DELETE, "DELETE", &one, auth, Value::Null, 403).await;
    }
    let (_, changed) = request(
        &router,
        UPDATE,
        "PUT",
        &one,
        admin_auth,
        json!({"name":"Renamed"}),
        200,
    )
    .await;
    assert_eq!(changed["rules"], rule["rules"]);
    assert_eq!(changed["conditions"], rule["conditions"]);
    assert_ne!(changed["updated_at"], rule["updated_at"]);
    request(&router, UPDATE, "PUT", &one, admin_auth, json!({}), 200).await;
    let history = format!("{one}/history");
    let (headers, versions) = request(
        &router,
        HISTORY,
        "GET",
        &format!("{history}?per_page=1"),
        reader_auth,
        Value::Null,
        200,
    )
    .await;
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    assert_eq!(versions[0]["actor"]["id"], admin);
    let (_, versions) = request(
        &router,
        HISTORY,
        "GET",
        &history,
        reader_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(
        versions.as_array().unwrap().len(),
        2,
        "Failed and no-op updates do not create history"
    );
    let version = versions[1]["version_id"].as_i64().unwrap();
    let (_, snapshot) = request(
        &router,
        VERSION,
        "GET",
        &format!("{history}/{version}"),
        reader_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(snapshot["state"]["name"], "Release policy");
    assert!(snapshot["state"].get("bypass_actors").is_none());
    let (_, snapshot) = request(
        &router,
        VERSION,
        "GET",
        &format!("{history}/{version}"),
        admin_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(snapshot["state"]["bypass_actors"], rule["bypass_actors"]);
    request(
        &router,
        VERSION,
        "GET",
        &format!("{history}/99999"),
        admin_auth,
        Value::Null,
        404,
    )
    .await;

    let (_,parent) = request(&router,"repos/create-org-ruleset","POST","/api/v3/orgs/acme/rulesets",owner_auth,json!({"name":"Inherited","enforcement":"disabled","conditions":{"repository_name":{"include":["~ALL"],"exclude":["other"]},"ref_name":{"include":["~ALL"]}},"bypass_actors":[{"actor_type":"OrganizationAdmin"}],"rules":[{"type":"deletion"}]}),201).await;
    let parent_id = parent["id"].as_i64().unwrap();
    for conditions in [
        json!({"repository_name":{"include":["other"]},"ref_name":{"include":["~ALL"]}}),
        json!({"repository_property":{"include":[{"name":"visibility","source":"system","property_values":["public"]}]},"ref_name":{"include":["~ALL"]}}),
    ] {
        request(
            &router,
            "repos/create-org-ruleset",
            "POST",
            "/api/v3/orgs/acme/rulesets",
            owner_auth,
            json!({"name":"Not inherited","enforcement":"disabled","conditions":conditions}),
            201,
        )
        .await;
    }
    let (headers, rows) = request(
        &router,
        LIST,
        "GET",
        &format!("{path}?per_page=1"),
        admin_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(rows[0]["id"], id);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, rows) = request(
        &router,
        LIST,
        "GET",
        &format!("{path}?per_page=1&page=2"),
        admin_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(rows[0]["id"], parent_id);
    assert!(
        rows[0].get("bypass_actors").is_none(),
        "Repo admin is not an org owner"
    );
    assert_eq!(rows[0]["source"], "acme");
    let (_, rows) = request(
        &router,
        LIST,
        "GET",
        &format!("{path}?includes_parents=false"),
        admin_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let parent_path = format!("{path}/{parent_id}");
    request(
        &router,
        GET,
        "GET",
        &parent_path,
        admin_auth,
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        GET,
        "GET",
        &format!("{parent_path}?includes_parents=false"),
        admin_auth,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        UPDATE,
        "PUT",
        &parent_path,
        owner_auth,
        json!({"name":"Wrong owner"}),
        404,
    )
    .await;
    request(
        &router,
        DELETE,
        "DELETE",
        &parent_path,
        owner_auth,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        HISTORY,
        "GET",
        &format!("{parent_path}/history"),
        owner_auth,
        Value::Null,
        404,
    )
    .await;
    let (_,tag) = request(&router,CREATE,"POST",path,admin_auth,json!({"name":"Tags","target":"tag","enforcement":"disabled","conditions":{"ref_name":{"include":["~ALL"]}},"rules":[{"type":"deletion"}]}),201).await;
    let (_, filtered) = request(
        &router,
        LIST,
        "GET",
        &format!("{path}?targets=tag&per_page=1"),
        reader_auth,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(filtered[0]["id"], tag["id"]);
    for targets in ["push", "repository"] {
        let (_, empty) = request(
            &router,
            LIST,
            "GET",
            &format!("{path}?targets={targets}"),
            reader_auth,
            Value::Null,
            200,
        )
        .await;
        assert_eq!(empty, json!([]));
    }
    request(
        &router,
        LIST,
        "GET",
        &format!("{path}?targets=unknown"),
        reader_auth,
        Value::Null,
        422,
    )
    .await;
    request(
        &router,
        LIST,
        "GET",
        &format!("{path}?page=0"),
        reader_auth,
        Value::Null,
        400,
    )
    .await;

    // gh ruleset list reads the same documents over GraphQL.
    let repo_list = include_str!("../../repositories/graphql/cli/ruleset-list-repo.graphql");
    let listed = graphql(
        &router,
        &credentials[3],
        repo_list,
        json!({"owner":"acme","repo":"private","limit":30,"includeParents":true}),
    )
    .await;
    assert_eq!(listed["totalCount"], 3);
    let nodes = listed["nodes"].as_array().unwrap();
    let node = |id: &Value| nodes.iter().find(|n| n["databaseId"] == *id).unwrap();
    assert_eq!(
        node(&rule["id"]),
        &json!({"databaseId":id,"name":"Renamed","target":"BRANCH","enforcement":"DISABLED",
            "source":{"__typename":"Repository","owner":"acme/private"},"rules":{"totalCount":1}})
    );
    assert_eq!(node(&tag["id"])["target"], "TAG");
    assert_eq!(
        node(&parent["id"])["source"],
        json!({"__typename":"Organization","owner":"acme"})
    );
    let first = graphql(
        &router,
        &credentials[3],
        repo_list,
        json!({"owner":"acme","repo":"private","limit":1,"includeParents":false}),
    )
    .await;
    assert_eq!(first["totalCount"], 2);
    assert_eq!(first["pageInfo"]["hasNextPage"], true);
    let rest = graphql(
        &router,
        &credentials[3],
        repo_list,
        json!({"owner":"acme","repo":"private","limit":1,"includeParents":false,
            "endCursor":first["pageInfo"]["endCursor"]}),
    )
    .await;
    assert_eq!(rest["pageInfo"]["hasNextPage"], false);
    assert_ne!(
        rest["nodes"][0]["databaseId"],
        first["nodes"][0]["databaseId"]
    );
    let org_list = graphql(
        &router,
        &credentials[0],
        include_str!("../../repositories/graphql/cli/ruleset-list-org.graphql"),
        json!({"login":"acme","limit":30,"includeParents":true}),
    )
    .await;
    assert_eq!(org_list["totalCount"], 3);
    assert!(org_list["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n["source"] == json!({"__typename":"Organization","owner":"acme"})));

    let public = "/api/v3/repos/owner/personal/rulesets";
    let (_, personal) = request(&router, CREATE, "POST", public, owner_auth, input, 201).await;
    let personal_id = personal["id"].as_i64().unwrap();
    let (_, visible) = request(
        &router,
        GET,
        "GET",
        &format!("{public}/{personal_id}"),
        None,
        Value::Null,
        200,
    )
    .await;
    assert!(visible.get("bypass_actors").is_none());
    request(&router, LIST, "GET", public, None, Value::Null, 200).await;
    request(
        &router,
        VERSION,
        "GET",
        &format!("{public}/{personal_id}/history/{version}"),
        None,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        UPDATE,
        "PUT",
        &format!("{public}/{personal_id}"),
        writer_auth,
        json!({"name":"Denied"}),
        403,
    )
    .await;
    request(
        &router,
        DELETE,
        "DELETE",
        &one,
        admin_auth,
        Value::Null,
        204,
    )
    .await;
    request(&router, GET, "GET", &one, admin_auth, Value::Null, 404).await;
    request(
        &router,
        HISTORY,
        "GET",
        &history,
        admin_auth,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        VERSION,
        "GET",
        &format!("{history}/{version}"),
        admin_auth,
        Value::Null,
        404,
    )
    .await;
    request(
        &router,
        DELETE,
        "DELETE",
        &one,
        admin_auth,
        Value::Null,
        404,
    )
    .await;
    f.finish().await;
}
#[tokio::test]
async fn github_repository_rulesets_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn github_repository_rulesets_postgres() {
    contract(true).await;
}
