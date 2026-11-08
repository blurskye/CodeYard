//! Real HTTP attestation conversations, including shared storage and live visibility, on both engines.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
mod access;
mod docker_conflicts;
mod inventory;
mod metadata;
mod pages;
mod users;

use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn token(f: &Database, router: &Router, actor: i64, scope: &str) -> String {
    let client = Client::login(router, f.pool(), actor).await;
    tokens::create(
        f.pool(),
        actor,
        Some(&client.session),
        TokenInput {
            name: "attestations",
            scope,
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .value
}

async fn oauth_token(f: &Database, router: &Router, actor: i64, scopes: &str) -> String {
    let value = token(f, router, actor, "write").await;
    let id = codeyard_db_identity::users::tokens::find_token(
        f.pool(),
        &codeyard_security::token_hash::token_hash(&value),
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    f.execute("INSERT INTO oauth_applications(owner_id,client_id,secret_hash,name,homepage,redirect_uri) VALUES($1,'package-fixture','fixture','Package fixture','https://example.test','https://example.test/callback') ON CONFLICT(client_id) DO NOTHING",&[actor]).await;
    f.execute(&format!("INSERT INTO oauth_app_tokens(token_id,client_id,scopes) VALUES($1,'package-fixture','{scopes}')"),&[id]).await;
    value
}

async fn call(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    input: Value,
    expected: u16,
) -> (HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(Body::from(if input.is_null() {
                    String::new()
                } else {
                    input.to_string()
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), expected, "{method} {path}");
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    if !value.is_null() {
        assert_github_schema(operation, expected, &value);
    }
    (headers, value)
}

fn bundle(digests: &[String], predicate: &str, signature: &str) -> Value {
    let subjects: Vec<_> = digests.iter().map(|digest| json!({"name":"artifact","digest":{"sha256":digest.strip_prefix("sha256:").unwrap()}})).collect();
    let payload = json!({"_type":"https://in-toto.io/Statement/v1","subject":subjects,"predicateType":predicate,"predicate":{}});
    json!({"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","verificationMaterial":{"certificate":{"rawBytes":STANDARD.encode("preserved, not trusted")}},"dsseEnvelope":{"payloadType":"application/vnd.in-toto+json","payload":STANDARD.encode(payload.to_string()),"signatures":[{"sig":STANDARD.encode(signature)}]}})
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("attestor").await;
    let other = f.actor("outsider").await;
    f.execute(
        "INSERT INTO organizations(name) VALUES('attested-org'),('other-org')",
        &[],
    )
    .await;
    let org = f
        .scalar(
            "SELECT id FROM organizations WHERE name='attested-org'",
            &[],
        )
        .await;
    let other_org = f
        .scalar("SELECT id FROM organizations WHERE name='other-org'", &[])
        .await;
    for org in [org, other_org] {
        f.execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
            &[org, owner],
        )
        .await;
    }
    f.execute("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,is_private) VALUES('org',$1,'public','/attested/public.git',false),('org',$1,'private','/attested/private.git',true)",&[org]).await;
    f.execute("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,is_private) VALUES('org',$1,'elsewhere','/attested/elsewhere.git',false)",&[other_org]).await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = token(&f, &router, owner, "write").await;
    let reader = token(&f, &router, owner, "read").await;
    let outsider = token(&f, &router, other, "write").await;
    let digest = format!("sha256:{}", "a".repeat(64));
    let second = format!("sha256:{}", "b".repeat(64));
    let input = json!({"bundle":bundle(&[digest.clone(),second.clone()],"https://slsa.dev/provenance/v1","first signature")});
    let create = "repos/create-attestation";
    let public = "/api/v3/repos/attested-org/public/attestations";
    call(
        &router,
        create,
        "POST",
        public,
        Some(&reader),
        input.clone(),
        403,
    )
    .await;
    call(
        &router,
        create,
        "POST",
        public,
        Some(&outsider),
        input.clone(),
        403,
    )
    .await;
    let unrelated = if pg {
        let mut tx = f.pg().begin().await.unwrap();
        codeyard_db_repos::postgres::attestations::lock(&mut tx, "org", other_org)
            .await
            .unwrap();
        Some(tx)
    } else {
        None
    };
    let (_, first) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        call(
            &router,
            create,
            "POST",
            public,
            Some(&writer),
            input.clone(),
            201,
        ),
    )
    .await
    .expect("an attestation in one org must not wait on another org's lock");
    if let Some(tx) = unrelated {
        tx.commit().await.unwrap();
    }
    let (_, repeat) = call(
        &router,
        create,
        "POST",
        public,
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    assert_eq!(repeat, first);
    let (_, private) = call(
        &router,
        create,
        "POST",
        "/api/v3/repos/attested-org/private/attestations",
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    call(
        &router,
        create,
        "POST",
        "/api/v3/repos/other-org/elsewhere/attestations",
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM attestation_bundles", &[])
            .await,
        1
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM attestation_subjects", &[])
            .await,
        6
    );
    let repo_list = format!("{public}/{digest}");
    let (_, listed) = call(
        &router,
        "repos/list-attestations",
        "GET",
        &repo_list,
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed["attestations"][0]["bundle"], input["bundle"]);
    call(
        &router,
        "repos/list-attestations",
        "GET",
        &repo_list,
        None,
        Value::Null,
        401,
    )
    .await;
    call(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("/api/v3/repos/attested-org/private/attestations/{digest}"),
        Some(&outsider),
        Value::Null,
        404,
    )
    .await;
    let org_list = format!("/api/v3/orgs/attested-org/attestations/{digest}");
    let (_, visible) = call(
        &router,
        "orgs/list-attestations",
        "GET",
        &org_list,
        Some(&outsider),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(visible["attestations"].as_array().unwrap().len(), 1);
    let (headers, page) = call(
        &router,
        "orgs/list-attestations",
        "GET",
        &format!("{org_list}?per_page=1&predicate_type=provenance"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(page["attestations"][0]["id"], first["id"]);
    let next = headers["link"]
        .to_str()
        .unwrap()
        .split('<')
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    let uri = url::Url::parse(next).unwrap();
    let (headers, page) = call(
        &router,
        "orgs/list-attestations",
        "GET",
        &format!("{}?{}", uri.path(), uri.query().unwrap()),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(page["attestations"][0]["id"], private["id"]);
    assert!(headers["link"].to_str().unwrap().contains("rel=\"prev\""));
    let (_, none) = call(
        &router,
        "orgs/list-attestations",
        "GET",
        &format!("{org_list}?predicate_type=sbom"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert!(none["attestations"].as_array().unwrap().is_empty());
    let (_, bulk) = call(
        &router,
        "orgs/list-attestations-bulk",
        "POST",
        "/api/v3/orgs/attested-org/attestations/bulk-list",
        Some(&reader),
        json!({"subject_digests":[digest,second],"predicate_type":"provenance"}),
        200,
    )
    .await;
    assert_eq!(
        bulk["attestations_subject_digests"][&digest]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        bulk["attestations_subject_digests"][&second]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let (_, repos) = call(
        &router,
        "orgs/list-attestation-repositories",
        "GET",
        "/api/v3/orgs/attested-org/attestations/repositories",
        Some(&outsider),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(repos.as_array().unwrap().len(), 1);
    assert_eq!(repos[0]["name"], "public");
    let mut malformed = input.clone();
    malformed["bundle"]["dsseEnvelope"]["payload"] = json!("not base64");
    call(
        &router,
        create,
        "POST",
        public,
        Some(&writer),
        malformed,
        422,
    )
    .await;
    let id_path = format!("/api/v3/orgs/attested-org/attestations/{}", private["id"]);
    call(
        &router,
        "orgs/delete-attestations-by-id",
        "DELETE",
        &id_path,
        Some(&reader),
        Value::Null,
        403,
    )
    .await;
    call(
        &router,
        "orgs/delete-attestations-by-id",
        "DELETE",
        &format!("/api/v3/orgs/other-org/attestations/{}", private["id"]),
        Some(&writer),
        Value::Null,
        404,
    )
    .await;
    call(
        &router,
        "orgs/delete-attestations-by-id",
        "DELETE",
        &id_path,
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    call(
        &router,
        "orgs/delete-attestations-by-subject-digest",
        "DELETE",
        &format!("/api/v3/orgs/attested-org/attestations/digest/{second}"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    let (_, new) = call(
        &router,
        create,
        "POST",
        public,
        Some(&writer),
        input.clone(),
        201,
    )
    .await;
    call(
        &router,
        "orgs/delete-attestations-bulk",
        "POST",
        "/api/v3/orgs/attested-org/attestations/delete-request",
        Some(&writer),
        json!({"attestation_ids":[new["id"]]}),
        200,
    )
    .await;
    let (_, other) = call(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("/api/v3/repos/other-org/elsewhere/attestations/{digest}"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(other["attestations"].as_array().unwrap().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_attestation_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_attestation_contract() {
    contract(true).await;
}

#[tokio::test]
async fn postgres_bundle_collection_skips_an_admitted_publisher() {
    let mut f = Database::new(true).await;
    f.execute(
        "INSERT INTO attestation_bundles(hash,bundle) VALUES('reserved-publisher','{}')",
        &[],
    )
    .await;
    let mut publisher = f.pg().begin().await.unwrap();
    sqlx::query("SELECT hash FROM attestation_bundles WHERE hash='reserved-publisher' FOR UPDATE")
        .fetch_one(&mut *publisher)
        .await
        .unwrap();
    let removed = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        codeyard_db_security::attestations::collect(f.pool()),
    )
    .await
    .expect("collection must skip busy bundles")
    .unwrap();
    assert_eq!(removed, 0);
    publisher.commit().await.unwrap();
    assert_eq!(
        codeyard_db_security::attestations::collect(f.pool())
            .await
            .unwrap(),
        1
    );
    f.finish().await;
}
