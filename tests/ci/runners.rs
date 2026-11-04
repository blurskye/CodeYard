//! REST runner administration uses real cluster pairing and the native claim transactions.
use crate::support::database::Database;
use crate::support::github_spec::assert_github_schema;
use crate::support::http_client::Client;
use crate::support::organizations::applied;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_repos_core::ci::runner_credentials::JitConfig;
use codeyard_ci::config::CiConfig;
use codeyard_ci::config::CiExecutionMode;

use codeyard_repos_core::context::NodeContext;
use codeyard_cluster_core::transport::Listen;
use codeyard_cluster_core::transport::NodeAddress;
use codeyard_cluster_core::transport::Relay;
use codeyard_support::node_identity::NodeKey;

use codeyard_db_core::models::OrgRole;
use codeyard_identity::tokens::TokenInput;
use codeyard_storage::ObjectStore;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt;

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    input: Value,
    expected: (&str, u16),
) -> Value {
    let reply = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", token)
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = reply.status();
    let cache = reply.headers().get("cache-control").cloned();
    let bytes = to_bytes(reply.into_body(), 8 * 1024 * 1024).await.unwrap();
    assert_eq!(
        status.as_u16(),
        expected.1,
        "{method} {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if expected.1 == 204 {
        assert!(bytes.is_empty());
        return Value::Null;
    }
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema(expected.0, expected.1, &body);
    if body.get("token").is_some() || body.get("encoded_jit_config").is_some() {
        assert_eq!(cache.unwrap(), "no-store");
    }
    body
}
async fn token(
    router: &Router,
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    scope: &str,
) -> String {
    let session = Client::login(router, pool, actor).await;
    format!(
        "Bearer {}",
        codeyard_identity::tokens::create(
            pool,
            actor,
            Some(&session.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None
            }
        )
        .await
        .unwrap()
        .value
    )
}
async fn pair(ticket: &str, key: &NodeKey, name: &str) {
    let ticket = codeyard_cluster_core::pairing::JoinTicket::decode(ticket).unwrap();
    let joined = codeyard_cluster_core::pairing::join(&ticket, key, &Relay::Off, name, None)
        .await
        .unwrap();
    assert!(joined.newly_joined);
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("runner_owner").await;
    let bob = f.actor("runner_member").await;
    let eve = f.actor("runner_outsider").await;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "runner-org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        codeyard_db_orgs::organizations::membership::add_organization_member(
            f.pool(),
            alice,
            org.id,
            bob,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let key = NodeKey::generate();
    let endpoint = codeyard_cluster_core::transport::bind(
        &Listen {
            bind: "127.0.0.1:0".parse().unwrap(),
            relay: Relay::Off,
            advertise: Vec::new(),
        },
        &key,
        f.pool().clone(),
    )
    .await
    .unwrap();
    let core = codeyard_cluster_core::transport::router(
        endpoint,
        codeyard_cluster_core::pairing::PairingHandler::new(f.pool().clone()),
    )
    .spawn();
    codeyard_cluster::wait_for_address(core.endpoint()).await;
    let address = NodeAddress::of(&core.endpoint().addr());
    let node = codeyard_db_cluster::cluster::register_own_node(
        f.pool(),
        &key.public_hex(),
        "runner-core",
        &["storage".into(), "web".into()],
        env!("CARGO_PKG_VERSION"),
    )
    .await
    .unwrap();
    codeyard_db_cluster::cluster::record_node_heartbeat(
        f.pool(),
        &key.public_hex(),
        env!("CARGO_PKG_VERSION"),
        &serde_json::to_string(&address).unwrap(),
        Some(1),
    )
    .await
    .unwrap();
    let context = NodeContext {
        pool: Some(f.pool().clone()),
        iroh: codeyard_cluster_core::transport::IrohDialer::new(Relay::Off, key.clone()),
        key,
        node_id: node.id,
        repos_path: root.path().to_owned(),
        peers: Default::default(),
        unreachable: Default::default(),
        ci_replicas: Default::default(),
    };
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig {
            mode: CiExecutionMode::Local,
            trusted_actor_ids: BTreeSet::from([alice]),
            ..CiConfig::default()
        },
        Some(context),
    ));
    let owner = Client::login(&router, f.pool(), alice).await;
    let writer = token(&router, f.pool(), alice, "write").await;
    let reader = token(&router, f.pool(), alice, "read").await;
    let member = token(&router, f.pool(), bob, "write").await;
    let outsider = token(&router, f.pool(), eve, "write").await;
    let repo = owner
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"one","is_private":true,"owner_org_id":org.id}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let personal = owner
        .expect(
            "POST",
            "/api/repos",
            json!({"name":"personal","is_private":true}),
            StatusCode::CREATED,
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let base = "/api/v3/orgs/runner-org/actions/runner-groups";
    let groups = call(
        &router,
        "GET",
        base,
        &reader,
        Value::Null,
        ("actions/list-self-hosted-runner-groups-for-org", 200),
    )
    .await;
    let default = groups["runner_groups"][0]["id"].as_i64().unwrap();
    assert_eq!(groups["runner_groups"][0]["default"], true);
    for token in [&reader, &member, &outsider] {
        call(
            &router,
            "POST",
            base,
            token,
            json!({"name":"Denied"}),
            ("actions/create-self-hosted-runner-group-for-org", 403),
        )
        .await;
    }
    call(
        &router,
        "POST",
        base,
        &writer,
        json!({"name":""}),
        ("actions/create-self-hosted-runner-group-for-org", 422),
    )
    .await;
    let group=call(&router,"POST",base,&writer,json!({"name":"Restricted","visibility":"selected","selected_repository_ids":[repo],"restricted_to_workflows":true,"selected_workflows":["runner-org/one/.github/workflows/ci.yml@main"]}),("actions/create-self-hosted-runner-group-for-org",201)).await;
    let gid = group["id"].as_i64().unwrap();
    call(
        &router,
        "POST",
        base,
        &writer,
        json!({"name":"rESTRICTED"}),
        ("actions/create-self-hosted-runner-group-for-org", 422),
    )
    .await;
    let paged = call(
        &router,
        "GET",
        &format!("{base}?page=2&per_page=1"),
        &reader,
        Value::Null,
        ("actions/list-self-hosted-runner-groups-for-org", 200),
    )
    .await;
    assert_eq!(paged["total_count"], 2);
    assert_eq!(paged["runner_groups"][0]["id"], gid);
    let group_path = format!("{base}/{gid}");
    let updated = call(
        &router,
        "PATCH",
        &group_path,
        &writer,
        json!({"allows_public_repositories":true}),
        ("actions/update-self-hosted-runner-group-for-org", 200),
    )
    .await;
    assert_eq!(updated["name"], "Restricted");
    assert_eq!(updated["allows_public_repositories"], true);
    call(
        &router,
        "GET",
        &group_path,
        &reader,
        Value::Null,
        ("actions/get-self-hosted-runner-group-for-org", 200),
    )
    .await;
    call(
        &router,
        "GET",
        &format!("{group_path}/hosted-runners"),
        &reader,
        Value::Null,
        ("actions/list-github-hosted-runners-in-group-for-org", 200),
    )
    .await;
    let repos = format!("{group_path}/repositories");
    assert_eq!(
        call(
            &router,
            "GET",
            &repos,
            &reader,
            Value::Null,
            (
                "actions/list-repo-access-to-self-hosted-runner-group-in-org",
                200
            )
        )
        .await["repositories"][0]["id"],
        repo
    );
    call(
        &router,
        "PUT",
        &repos,
        &writer,
        json!({"selected_repository_ids":[personal]}),
        (
            "actions/set-repo-access-to-self-hosted-runner-group-in-org",
            422,
        ),
    )
    .await;
    call(
        &router,
        "PUT",
        &repos,
        &writer,
        json!({"selected_repository_ids":[]}),
        (
            "actions/set-repo-access-to-self-hosted-runner-group-in-org",
            204,
        ),
    )
    .await;
    let visible = call(
        &router,
        "GET",
        &format!("{base}?visible_to_repository=one"),
        &reader,
        Value::Null,
        ("actions/list-self-hosted-runner-groups-for-org", 200),
    )
    .await;
    assert_eq!(visible["total_count"], 1);
    call(
        &router,
        "PUT",
        &format!("{repos}/{repo}"),
        &writer,
        Value::Null,
        (
            "actions/add-repo-access-to-self-hosted-runner-group-in-org",
            204,
        ),
    )
    .await;
    call(
        &router,
        "DELETE",
        &format!("{repos}/{repo}"),
        &writer,
        Value::Null,
        (
            "actions/remove-repo-access-to-self-hosted-runner-group-in-org",
            204,
        ),
    )
    .await;
    call(
        &router,
        "PUT",
        &repos,
        &writer,
        json!({"selected_repository_ids":[repo]}),
        (
            "actions/set-repo-access-to-self-hosted-runner-group-in-org",
            204,
        ),
    )
    .await;
    let extra = call(
        &router,
        "POST",
        base,
        &writer,
        json!({"name":"Temporary"}),
        ("actions/create-self-hosted-runner-group-for-org", 201),
    )
    .await["id"]
        .as_i64()
        .unwrap();
    call(
        &router,
        "PATCH",
        &format!("{base}/{extra}"),
        &writer,
        json!({"name":"Renamed"}),
        ("actions/update-self-hosted-runner-group-for-org", 200),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("{base}?per_page=1&page=2"),
            &reader,
            Value::Null,
            ("actions/list-self-hosted-runner-groups-for-org", 200)
        )
        .await["runner_groups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    call(
        &router,
        "DELETE",
        &format!("{base}/{default}"),
        &writer,
        Value::Null,
        ("actions/delete-self-hosted-runner-group-from-org", 409),
    )
    .await;
    let org_runner_key = NodeKey::generate();
    let repo_runner_key = NodeKey::generate();
    let mut runner_ids = Vec::new();
    let mut registration_tokens = Vec::new();
    let mut removal_tokens = Vec::new();
    for (scope, key, name) in [
        ("org", &org_runner_key, "org-worker"),
        ("repo", &repo_runner_key, "repo-worker"),
    ] {
        let base = if scope == "org" {
            "/api/v3/orgs/runner-org/actions/runners"
        } else {
            "/api/v3/repos/runner_owner/personal/actions/runners"
        };
        let operation = |name: &str| format!("actions/{name}-for-{scope}");
        call(
            &router,
            "GET",
            base,
            &outsider,
            Value::Null,
            (
                &operation("list-self-hosted-runners"),
                if scope == "org" { 403 } else { 404 },
            ),
        )
        .await;
        call(
            &router,
            "POST",
            &format!("{base}/registration-token"),
            &reader,
            Value::Null,
            (&operation("create-registration-token"), 403),
        )
        .await;
        let registration = call(
            &router,
            "POST",
            &format!("{base}/registration-token"),
            &writer,
            Value::Null,
            (&operation("create-registration-token"), 201),
        )
        .await;
        let token = registration["token"].as_str().unwrap().to_owned();
        pair(&token, key, name).await;
        registration_tokens.push(token);
        codeyard_db_cluster::cluster::touch_node(f.pool(), &key.public_hex())
            .await
            .unwrap();
        let rows = call(
            &router,
            "GET",
            base,
            &reader,
            Value::Null,
            (&operation("list-self-hosted-runners"), 200),
        )
        .await;
        let id = rows["runners"][0]["id"].as_i64().unwrap();
        let filtered = call(
            &router,
            "GET",
            &format!("{base}?name={name}&per_page=1"),
            &reader,
            Value::Null,
            (&operation("list-self-hosted-runners"), 200),
        )
        .await;
        assert_eq!(filtered["total_count"], 1);
        assert_eq!(filtered["runners"][0]["id"], id);
        let filtered = call(
            &router,
            "GET",
            &format!("{base}?name=absent&page=2&per_page=1"),
            &reader,
            Value::Null,
            (&operation("list-self-hosted-runners"), 200),
        )
        .await;
        assert_eq!(filtered["total_count"], 0);
        runner_ids.push(id);
        let path = format!("{base}/{id}");
        assert_eq!(
            call(
                &router,
                "GET",
                &path,
                &reader,
                Value::Null,
                (&operation("get-self-hosted-runner"), 200)
            )
            .await["status"],
            "online"
        );
        call(
            &router,
            "GET",
            &format!("{base}/deprecations/{}", env!("CARGO_PKG_VERSION")),
            &reader,
            Value::Null,
            (&operation("get-runner-version-deprecation"), 200),
        )
        .await;
        let apps = call(
            &router,
            "GET",
            &format!("{base}/downloads"),
            &reader,
            Value::Null,
            (&operation("list-runner-applications"), 200),
        )
        .await;
        assert!(apps[0]["download_url"]
            .as_str()
            .unwrap()
            .ends_with("/api/actions/runners/download"));
        let labels = format!("{path}/labels");
        call(
            &router,
            "GET",
            &labels,
            &reader,
            Value::Null,
            (&operation("list-labels-for-self-hosted-runner"), 200),
        )
        .await;
        call(
            &router,
            "POST",
            &labels,
            &writer,
            json!({"labels":["linux","custom","CUSTOM"]}),
            (&operation("add-custom-labels-to-self-hosted-runner"), 200),
        )
        .await;
        let set = call(
            &router,
            "PUT",
            &labels,
            &writer,
            json!({"labels":["linux","other"]}),
            (&operation("set-custom-labels-for-self-hosted-runner"), 200),
        )
        .await;
        assert_eq!(set["total_count"], 3);
        call(
            &router,
            "DELETE",
            &format!("{labels}/other"),
            &writer,
            Value::Null,
            (
                &operation("remove-custom-label-from-self-hosted-runner"),
                200,
            ),
        )
        .await;
        let cleared = call(
            &router,
            "DELETE",
            &labels,
            &writer,
            Value::Null,
            (
                &operation("remove-all-custom-labels-from-self-hosted-runner"),
                200,
            ),
        )
        .await;
        assert_eq!(cleared["labels"][0]["type"], "read-only");
        assert_eq!(cleared["total_count"], 1);
        call(
            &router,
            "DELETE",
            &format!("{labels}/self-hosted"),
            &writer,
            Value::Null,
            (
                &operation("remove-custom-label-from-self-hosted-runner"),
                404,
            ),
        )
        .await;
        call(
            &router,
            "POST",
            &labels,
            &writer,
            json!({"labels":["bad label"]}),
            (&operation("add-custom-labels-to-self-hosted-runner"), 422),
        )
        .await;
        call(
            &router,
            "PUT",
            &labels,
            &writer,
            json!({"labels":[42]}),
            (&operation("set-custom-labels-for-self-hosted-runner"), 422),
        )
        .await;
        call(
            &router,
            "POST",
            &labels,
            &writer,
            json!({"labels":["linux"]}),
            (&operation("add-custom-labels-to-self-hosted-runner"), 200),
        )
        .await;
        let removal = call(
            &router,
            "POST",
            &format!("{base}/remove-token"),
            &writer,
            Value::Null,
            (&operation("create-remove-token"), 201),
        )
        .await;
        removal_tokens.push(removal["token"].as_str().unwrap().to_owned());
    }
    let group_runners = format!("{group_path}/runners");
    let org_runner = runner_ids[0];
    let repo_runner = runner_ids[1];
    call(
        &router,
        "PUT",
        &group_runners,
        &writer,
        json!({"runners":[repo_runner]}),
        ("actions/set-self-hosted-runners-in-group-for-org", 422),
    )
    .await;
    call(
        &router,
        "PUT",
        &group_runners,
        &writer,
        json!({"runners":[org_runner]}),
        ("actions/set-self-hosted-runners-in-group-for-org", 204),
    )
    .await;
    assert_eq!(
        call(
            &router,
            "GET",
            &group_runners,
            &reader,
            Value::Null,
            ("actions/list-self-hosted-runners-in-group-for-org", 200)
        )
        .await["total_count"],
        1
    );
    call(
        &router,
        "DELETE",
        &format!("{group_runners}/{org_runner}"),
        &writer,
        Value::Null,
        ("actions/remove-self-hosted-runner-from-group-for-org", 204),
    )
    .await;
    call(
        &router,
        "PUT",
        &format!("{group_runners}/{org_runner}"),
        &writer,
        Value::Null,
        ("actions/add-self-hosted-runner-to-group-for-org", 204),
    )
    .await;
    call(
        &router,
        "DELETE",
        &format!("{base}/{extra}"),
        &writer,
        Value::Null,
        ("actions/delete-self-hosted-runner-group-from-org", 204),
    )
    .await;
    let settings = format!("/settings/actions/runners?org={}", org.id);
    let html = owner.page_ok(&settings).await;
    assert!(html.contains("Just-in-time runner"));
    assert!(!html.contains(&registration_tokens[0]));
    let (_, model) = owner.model(&settings).await;
    let codeyard_webui::app::Page::RunnerSettings(page) = model else {
        panic!("runner page")
    };
    assert!(page
        .runners
        .iter()
        .any(|r| r.id == org_runner && r.labels == "linux"));
    let saved = owner
        .form_reply(
            &settings,
            &[
                ("action", "labels"),
                ("id", &org_runner.to_string()),
                ("labels", "linux, native"),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
    let invalid = owner
        .form_reply(
            &settings,
            &[
                ("action", "group-create"),
                ("name", "Draft group"),
                ("visibility", "selected"),
                ("repositories", &personal.to_string()),
            ],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(invalid.text().contains("Draft group"));
    let created = owner
        .form_reply(
            &settings,
            &[
                ("action", "group-create"),
                ("name", "Native group"),
                ("visibility", "private"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    let credential = owner
        .form_reply(&settings, &[("action", "registration-token")])
        .await;
    assert_eq!(credential.status, StatusCode::OK);
    assert!(
        serde_json::from_slice::<Value>(&credential.body).unwrap()["token"]
            .as_str()
            .unwrap()
            .starts_with("codeyard-join-v1.")
    );
    // Actual workflow admission records group and label selectors; an unrelated runner
    // cannot claim it, and a policy change cancels the claim before another checkout.
    let yaml="name: Scoped\non: workflow_dispatch\npermissions: {actions: write}\njobs:\n  check:\n    runs-on: {group: Restricted, labels: [self-hosted, linux]}\n    steps: [{run: echo scoped}]\n";
    call(
        &router,
        "PUT",
        "/api/v3/repos/runner-org/one/contents/.github/workflows/ci.yml",
        &writer,
        json!({"message":"Add runner workflow","content":STANDARD.encode(yaml)}),
        ("repos/create-or-update-file-contents", 201),
    )
    .await;
    let dispatched=owner.expect("POST","/api/ci/trigger",json!({"repo_owner":"runner-org","repo_name":"one","branch":"main","event":"workflow_dispatch","pipeline":"ci"}),StatusCode::OK).await;
    let run_id = dispatched["job_id"]
        .as_str()
        .unwrap()
        .parse::<uuid::Uuid>()
        .unwrap();
    assert!(codeyard_db_actions::ci::claim_ci_run_on_node(
        f.pool(),
        &repo_runner_key.public_hex(),
        &format!("{}/slot", repo_runner_key.public_hex()),
        "local",
        &[alice],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .is_none());
    let worker = format!("{}/slot", org_runner_key.public_hex());
    let claim = codeyard_db_actions::ci::claim_ci_run_on_node(
        f.pool(),
        &org_runner_key.public_hex(),
        &worker,
        "local",
        &[alice],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claim.run_id, run_id);
    assert_eq!(claim.runner_group.as_deref(), Some("Restricted"));
    // actions:write is not runner-administration authority. In particular, an
    // active workflow token cannot mint a longer-lived registration credential.
    let credentials = codeyard_ci::tokens::issue(
        f.pool(),
        &claim,
        &std::collections::BTreeMap::from([(
            ".github/workflows/ci.yml".to_owned(),
            yaml.as_bytes().to_vec(),
        )]),
        false,
    )
    .await
    .unwrap();
    let job_token = &credentials.workflows[".github/workflows/ci.yml"]["check"];
    let job_auth = format!("Bearer {job_token}");
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v3/user",
            &job_auth,
            Value::Null,
            ("users/get-authenticated", 200)
        )
        .await["login"],
        codeyard_db_core::queries::ci_tokens::BOT
    );
    let repo_runners = "/api/v3/repos/runner-org/one/actions/runners";
    for (suffix, operation, body) in [
        (
            "registration-token",
            "actions/create-registration-token-for-repo",
            Value::Null,
        ),
        (
            "remove-token",
            "actions/create-remove-token-for-repo",
            Value::Null,
        ),
        (
            "generate-jitconfig",
            "actions/generate-runner-jitconfig-for-repo",
            json!({"name":"Denied","runner_group_id":1,"labels":["linux"]}),
        ),
    ] {
        call(
            &router,
            "POST",
            &format!("{repo_runners}/{suffix}"),
            &job_auth,
            body,
            (operation, 403),
        )
        .await;
    }
    assert!(codeyard_db_actions::runners::list_runners(
        f.pool(),
        alice,
        codeyard_db_core::queries::runners::Scope::Repository(repo),
        &Default::default()
    )
    .await
    .unwrap()
    .rows
    .is_empty());
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("/api/v3/orgs/runner-org/actions/runners/{org_runner}"),
            &reader,
            Value::Null,
            ("actions/get-self-hosted-runner-for-org", 200)
        )
        .await["busy"],
        true
    );
    call(
        &router,
        "DELETE",
        &format!("{repos}/{repo}"),
        &writer,
        Value::Null,
        (
            "actions/remove-repo-access-to-self-hosted-runner-group-in-org",
            204,
        ),
    )
    .await;
    assert_eq!(
        codeyard_db_actions::ci::heartbeat_ci_run(f.pool(), claim.id, &worker)
            .await
            .unwrap(),
        Some(true)
    );
    assert!(!codeyard_db_actions::ci::ci_run_allows_repository_read(
        f.pool(),
        claim.id,
        repo,
        &worker,
        &org_runner_key.public_hex()
    )
    .await
    .unwrap());
    assert!(codeyard_db_actions::ci::claim_ci_run_on_node(
        f.pool(),
        &org_runner_key.public_hex(),
        &worker,
        "local",
        &[alice],
        &codeyard_ci::config::CiConfig::default().platform_labels(),
    )
    .await
    .unwrap()
    .is_none());
    codeyard_db_actions::ci::finish_ci_run(f.pool(), claim.id, &worker, "cancelled", None)
        .await
        .unwrap();
    call(
        &router,
        "PUT",
        &format!("{repos}/{repo}"),
        &writer,
        Value::Null,
        (
            "actions/add-repo-access-to-self-hosted-runner-group-in-org",
            204,
        ),
    )
    .await;
    for scope in ["org", "repo"] {
        let (base, group_id) = if scope == "org" {
            ("/api/v3/orgs/runner-org/actions/runners", gid)
        } else {
            ("/api/v3/repos/runner_owner/personal/actions/runners", 1)
        };
        let op = format!("actions/generate-runner-jitconfig-for-{scope}");
        call(
            &router,
            "POST",
            &format!("{base}/generate-jitconfig"),
            &writer,
            json!({"name":"Incomplete","runner_group_id":group_id}),
            (&op, 422),
        )
        .await;
        call(&router,"POST",&format!("{base}/generate-jitconfig"),&writer,json!({"name":"Invalid","runner_group_id":group_id,"labels":["linux"],"work_folder":"../escape"}),(&op,422)).await;
        let jit=call(&router,"POST",&format!("{base}/generate-jitconfig"),&writer,json!({"name":format!("Disposable {scope}"),"runner_group_id":group_id,"labels":["linux"],"work_folder":"jit-work"}),(&op,201)).await;
        let encoded = jit["encoded_jit_config"].as_str().unwrap();
        let config = JitConfig::decode(encoded).unwrap();
        let jit_key = NodeKey::generate();
        if scope == "repo" {
            let state = root.path().join("jit-cli-state");
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(90),
                tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
                    .env_clear()
                    .env("PATH", std::env::var_os("PATH").unwrap())
                    .env("HOME", root.path())
                    .env("GITSERV_STATE_DIR", &state)
                    .env("CODEYARD_RUNNER_JIT_CONFIG", encoded)
                    .env("IROH_RELAY", "")
                    .env("IROH_BIND", "127.0.0.1:0")
                    .current_dir(root.path())
                    .args(["cluster", "join"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("JIT CLI join timed out")
            .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                codeyard_repos_core::ci::runner_credentials::work_root(&state).unwrap(),
                state.join("runner-workspaces/jit-work")
            );
            let node = codeyard_db_cluster::cluster::get_node_by_name(f.pool(), &config.node_name)
                .await
                .unwrap()
                .unwrap();
            let runner = codeyard_db_actions::runners::list_runners(
                f.pool(),
                alice,
                codeyard_db_core::queries::runners::Scope::Repository(personal),
                &Default::default(),
            )
            .await
            .unwrap()
            .rows
            .into_iter()
            .find(|r| r.id == jit["runner"]["id"].as_i64().unwrap())
            .unwrap();
            assert_eq!(runner.node_id, Some(node.id));
            assert!(!String::from_utf8_lossy(&output.stdout).contains(&config.ticket));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(encoded));
        } else {
            pair(&config.ticket, &jit_key, &config.node_name).await;
        }
        let retry_key = NodeKey::generate();
        assert!(codeyard_cluster_core::pairing::join(
            &codeyard_cluster_core::pairing::JoinTicket::decode(&config.ticket).unwrap(),
            &retry_key,
            &Relay::Off,
            "second-use",
            None
        )
        .await
        .is_err());
        if scope == "org" {
            let dispatched=owner.expect("POST","/api/ci/trigger",json!({"repo_owner":"runner-org","repo_name":"one","branch":"main","event":"workflow_dispatch","pipeline":"ci"}),StatusCode::OK).await;
            let jit_worker = format!("{}/slot", jit_key.public_hex());
            let run = codeyard_db_actions::ci::claim_ci_run_on_node(
                f.pool(),
                &jit_key.public_hex(),
                &jit_worker,
                "local",
                &[alice],
                &codeyard_ci::config::CiConfig::default().platform_labels(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                run.run_id.to_string(),
                dispatched["job_id"].as_str().unwrap()
            );
            assert!(codeyard_db_actions::ci::claim_ci_run_on_node(
                f.pool(),
                &jit_key.public_hex(),
                &format!("{}/second-slot", jit_key.public_hex()),
                "local",
                &[alice],
                &codeyard_ci::config::CiConfig::default().platform_labels(),
            )
            .await
            .unwrap()
            .is_none());
            codeyard_db_actions::ci::finish_ci_run(f.pool(), run.id, &jit_worker, "success", None)
                .await
                .unwrap();
            assert!(
                !codeyard_db_cluster::cluster::is_active_node(f.pool(), &jit_key.public_hex())
                    .await
                    .unwrap()
            );
        }
    }
    // Remove tokens are scoped and redeem against the same registered workers.
    let wrong = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/actions/runners/{org_runner}/remove"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"token":removal_tokens[1]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
    let removable_key = NodeKey::generate();
    pair(&registration_tokens[0], &removable_key, "removable").await;
    let removable = codeyard_db_actions::runners::list_runners(
        f.pool(),
        alice,
        codeyard_db_core::queries::runners::Scope::Organization(org.id),
        &Default::default(),
    )
    .await
    .unwrap()
    .rows
    .into_iter()
    .find(|r| r.name == "removable")
    .unwrap();
    let removed = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/actions/runners/{}/remove", removable.id))
                .header("content-type", "application/json")
                .body(Body::from(json!({"token":removal_tokens[0]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert!(
        !codeyard_db_cluster::cluster::is_active_node(f.pool(), &removable_key.public_hex())
            .await
            .unwrap()
    );
    for (scope, id) in [("org", org_runner), ("repo", repo_runner)] {
        let base = if scope == "org" {
            "/api/v3/orgs/runner-org/actions/runners"
        } else {
            "/api/v3/repos/runner_owner/personal/actions/runners"
        };
        call(
            &router,
            "DELETE",
            &format!("{base}/{id}"),
            &writer,
            Value::Null,
            (
                &format!("actions/delete-self-hosted-runner-from-{scope}"),
                204,
            ),
        )
        .await;
        call(
            &router,
            "GET",
            &format!("{base}/{id}"),
            &reader,
            Value::Null,
            (&format!("actions/get-self-hosted-runner-for-{scope}"), 404),
        )
        .await;
    }
    f.execute(
        "UPDATE join_tokens SET expires_at='2000-01-01T00:00:00Z' WHERE runner_scoped",
        &[],
    )
    .await;
    let expired_key = NodeKey::generate();
    assert!(codeyard_cluster_core::pairing::join(
        &codeyard_cluster_core::pairing::JoinTicket::decode(&registration_tokens[0]).unwrap(),
        &expired_key,
        &Relay::Off,
        "expired",
        None
    )
    .await
    .is_err());
    // A transfer drops the repository's own runners, which the former owner registered.
    f.execute(
        "INSERT INTO action_runners(repo_id,name) VALUES($1,'Moving')",
        &[personal],
    )
    .await;
    codeyard_db_repos::repositories::records::transfer_repository(
        f.pool(),
        alice,
        personal,
        codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(org.id),
    )
    .await
    .unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM action_runners WHERE repo_id=$1",
            &[personal]
        )
        .await,
        0
    );
    core.shutdown().await.unwrap();
    drop(router);
    f.finish().await;
}
#[tokio::test]
#[ignore = "requires the built web UI and Chromium; run explicitly on the builder"]
async fn runner_settings_browser() {
    let mut f = Database::new(false).await;
    f.complete_setup().await;
    let alice = f.actor("browser_owner").await;
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "browser-org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let result = tokio::process::Command::new("python3")
        .arg("scripts/test-runners-browser.py")
        .arg(&base)
        .arg(org.id.to_string())
        .env("CODEYARD_TEST_SESSION", &session.session)
        .output()
        .await
        .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    println!("{}", String::from_utf8_lossy(&result.stdout));
    let groups =
        codeyard_db_actions::runners::list_groups(f.pool(), alice, org.id, &Default::default())
            .await
            .unwrap()
            .rows;
    assert_eq!(groups.len(), 1);
    assert!(groups[0].is_default);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_runner_administration() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_runner_administration() {
    contract(true).await;
}
