//! gh 2.102.0 requests pass through the application router on both engines.
use super::{
    action_secrets::{pat, sealed},
    actions_objects, results,
};
use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, crypto::SecretEncryptor, db,
    repositories::lifecycle, storage::ObjectStore,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt;

async fn raw(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
    expected: u16,
) -> (HeaderMap, Vec<u8>) {
    let path = url::Url::parse(path)
        .map(|url| {
            format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        })
        .unwrap_or_else(|_| path.to_owned());
    let mut req = Request::builder().method(method).uri(&path);
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let body = if let Some(body) = body {
        req = req.header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let reply = router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = reply.status();
    let headers = reply.headers().clone();
    let bytes = to_bytes(reply.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    assert_eq!(
        status.as_u16(),
        expected,
        "{method} {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    (headers, bytes)
}

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
    expected: u16,
) -> Value {
    let (_, bytes) = raw(router, method, path, Some(token), body, expected).await;
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn timestamp(value: &Value) {
    chrono::DateTime::parse_from_rfc3339(value.as_str().unwrap()).unwrap();
}

fn run_fields(run: &Value, id: i64, workflow: i64, sha: &str, expected: (&str, Value, i64)) {
    let (status, conclusion, attempt) = expected;
    assert_eq!(run["id"], id);
    assert_eq!(run["name"], "Continuous integration");
    assert_eq!(run["display_title"], "Install workflow");
    for field in ["created_at", "updated_at"] {
        timestamp(&run[field]);
    }
    if !run["run_started_at"].is_null() {
        timestamp(&run["run_started_at"]);
    }
    assert_eq!(run["status"], status);
    assert_eq!(run["conclusion"], conclusion);
    assert_eq!(run["event"], "workflow_dispatch");
    assert_eq!(run["workflow_id"], workflow);
    assert_eq!(run["run_number"], id);
    assert_eq!(run["run_attempt"], attempt);
    assert_eq!(run["head_branch"], "main");
    assert!(run["jobs_url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/api/v3/repos/alice/proj/actions/runs/{id}/jobs")));
    assert_eq!(run["head_commit"]["message"], "Install workflow");
    assert_eq!(run["head_sha"], sha);
    assert!(run["html_url"]
        .as_str()
        .unwrap()
        .contains("/alice/proj/actions?job="));
    assert_eq!(run["head_repository"]["owner"]["login"], "alice");
    assert_eq!(run["head_repository"]["name"], "proj");
}

const DEFAULT_BRANCH: &str = r#"
    fragment repo on Repository {
        id
        databaseId
        name
        owner { login }
        sshUrl
        hasIssuesEnabled
        description
        hasWikiEnabled
        viewerPermission
        defaultBranchRef {
            name
        }
    }

    query RepositoryInfo($owner: String!, $name: String!) {
        repository(owner: $owner, name: $name) {
            ...repo
            parent {
                ...repo
            }
            mergeCommitAllowed
            rebaseMergeAllowed
            squashMergeAllowed
        }
    }"#;

const PULL_REQUEST: &str = r#"
        query PullRequestForRun($owner: String!, $repo: String!, $headRefName: String!) {
            repository(owner: $owner, name: $repo) {
                pullRequests(headRefName: $headRefName, first: 1, orderBy: { field: CREATED_AT, direction: DESC }) {
                    nodes {
                        number
                        headRepository {
                            owner {
                                login
                            }
                            name
                        }
                    }
                }
            }
        }"#;

async fn graphql(router: &Router, token: &str, query: &str, variables: Value) -> Value {
    let reply = request(
        router,
        "POST",
        "/api/graphql",
        token,
        Some(json!({"query":query,"variables":variables})),
        200,
    )
    .await;
    assert!(reply.get("errors").is_none(), "{reply}");
    reply["data"].clone()
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let encryptor = Arc::new(SecretEncryptor::new(&[0x45; 32]));
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor),
        CiConfig {
            trusted_actor_ids: BTreeSet::from([alice]),
            ..Default::default()
        },
        None,
    );
    let router = create_router(state);
    let writer = pat(&f, &router, alice, "write").await;
    let reader = pat(&f, &router, alice, "read").await;
    let outsider = pat(&f, &router, bob, "write").await;
    let client = Client::login(&router, f.pool(), alice).await;
    let workflow = "name: Continuous integration\non:\n  workflow_dispatch:\n    inputs:\n      target:\n        type: string\n        default: test\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo ok}]\n";
    let saved = client.expect("POST","/api/repos/alice/proj/contents/.github/workflows/ci.yml",json!({"content":STANDARD.encode(workflow),"message":"Install workflow","branch":"main","expected_revision":"unborn"}),axum::http::StatusCode::CREATED).await;
    let sha = saved["sha"].as_str().unwrap();
    let base = "/api/v3/repos/alice/proj/actions";
    let meta = request(&router, "GET", "/api/v3/meta", &reader, None, 200).await;
    assert_eq!(meta["installed_version"], env!("CARGO_PKG_VERSION"));
    let data = graphql(
        &router,
        &writer,
        DEFAULT_BRANCH,
        json!({"owner":"alice","name":"proj"}),
    )
    .await;
    let info = &data["repository"];
    assert_eq!(info["defaultBranchRef"]["name"], "main");
    assert_eq!(info["databaseId"], repo.id);
    assert!(info["id"].is_string());
    assert_eq!(info["name"], "proj");
    assert_eq!(info["owner"]["login"], "alice");
    assert!(info["sshUrl"]
        .as_str()
        .unwrap()
        .ends_with("/alice/proj.git"));
    assert_eq!(info["hasIssuesEnabled"], true);
    assert_eq!(info["hasWikiEnabled"], false);
    assert_eq!(info["viewerPermission"], "ADMIN");
    assert!(info["description"].is_null() && info["parent"].is_null());
    assert_eq!(info["mergeCommitAllowed"], repo.allow_merge_commit);
    assert_eq!(info["rebaseMergeAllowed"], repo.allow_rebase_merge);
    assert_eq!(info["squashMergeAllowed"], repo.allow_squash_merge);
    let mapped = graphql(&router,&writer,"query MapRepositoryNames { repo_000: repository(owner: \"alice\", name: \"proj\") { databaseId } }",json!({})).await;
    assert_eq!(mapped["repo_000"]["databaseId"], repo.id);
    for limit in [50, 100] {
        let workflows = request(
            &router,
            "GET",
            &format!("{base}/workflows?per_page={limit}&page=1"),
            &reader,
            None,
            200,
        )
        .await;
        assert_eq!(workflows["workflows"].as_array().unwrap().len(), 1);
        let workflow = &workflows["workflows"][0];
        assert_eq!(workflow["name"], "Continuous integration");
        assert_eq!(workflow["path"], ".github/workflows/ci.yml");
        assert_eq!(workflow["state"], "active");
        assert!(workflow["id"].as_i64().unwrap() > 0);
    }
    let workflow_info = request(
        &router,
        "GET",
        &format!("{base}/workflows/ci.yml"),
        &reader,
        None,
        200,
    )
    .await;
    let wid = workflow_info["id"].as_i64().unwrap();
    assert_eq!(
        request(
            &router,
            "GET",
            &format!("{base}/workflows/{wid}"),
            &reader,
            None,
            200
        )
        .await,
        workflow_info
    );
    assert!(request(
        &router,
        "GET",
        &format!("{base}/workflows?per_page=1&page=2"),
        &reader,
        None,
        200
    )
    .await["workflows"]
        .as_array()
        .unwrap()
        .is_empty());
    for suffix in ["", "?ref=main"] {
        let content = request(
            &router,
            "GET",
            &format!("/api/v3/repos/alice/proj/contents/.github/workflows/ci.yml{suffix}"),
            &reader,
            None,
            200,
        )
        .await;
        assert_eq!(
            STANDARD
                .decode(content["content"].as_str().unwrap().replace('\n', ""))
                .unwrap(),
            workflow.as_bytes()
        );
    }
    request(
        &router,
        "POST",
        &format!("{base}/workflows/{wid}/dispatches"),
        &reader,
        Some(json!({"ref":"main","inputs":{}})),
        403,
    )
    .await;
    request(
        &router,
        "POST",
        &format!("{base}/workflows/{wid}/dispatches"),
        &writer,
        Some(json!({"ref":"main","inputs":{"unknown":"no"}})),
        422,
    )
    .await;
    for inputs in [json!({}), json!({"target":"production"})] {
        request(
            &router,
            "POST",
            &format!("{base}/workflows/{wid}/dispatches"),
            &writer,
            Some(json!({"ref":"main","inputs":inputs})),
            204,
        )
        .await;
    }
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
    let id = runs.iter().map(|run| run.id).max().unwrap();
    let listing = request(
        &router,
        "GET",
        &format!("{base}/runs?per_page=20&exclude_pull_requests=true"),
        &reader,
        None,
        200,
    )
    .await;
    assert_eq!(listing["total_count"], 2);
    run_fields(
        &listing["workflow_runs"][0],
        id,
        wid,
        sha,
        ("queued", Value::Null, 1),
    );
    for suffix in [
        format!("runs/{id}?exclude_pull_requests=true"),
        format!("runs/{id}/attempts/1?exclude_pull_requests=true"),
    ] {
        run_fields(
            &request(
                &router,
                "GET",
                &format!("{base}/{suffix}"),
                &reader,
                None,
                200,
            )
            .await,
            id,
            wid,
            sha,
            ("queued", Value::Null, 1),
        );
    }
    request(
        &router,
        "GET",
        &format!("{base}/runs/{id}/attempts/2?exclude_pull_requests=true"),
        &reader,
        None,
        404,
    )
    .await;
    for limit in [5, 10, 50, 100] {
        let listing = request(
            &router,
            "GET",
            &format!("{base}/workflows/{wid}/runs?per_page={limit}&exclude_pull_requests=true"),
            &reader,
            None,
            200,
        )
        .await;
        assert_eq!(listing["total_count"], 2);
        run_fields(
            &listing["workflow_runs"][0],
            id,
            wid,
            sha,
            ("queued", Value::Null, 1),
        );
    }
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    for filter in [
        "branch=main".to_owned(),
        "actor=alice".into(),
        "event=workflow_dispatch".into(),
        "status=queued".into(),
        format!("head_sha={sha}"),
        format!("created={today}"),
        format!("created={today}..{today}"),
        format!("created=%3E%3D{today}"),
    ] {
        assert_eq!(
            request(
                &router,
                "GET",
                &format!("{base}/runs?per_page=20&exclude_pull_requests=true&{filter}"),
                &reader,
                None,
                200
            )
            .await["total_count"],
            2,
            "{filter}"
        );
    }
    for filter in [
        "branch=absent",
        "actor=bob",
        "event=push",
        "status=success",
        "head_sha=0000000000000000000000000000000000000000",
        "created=2000-01-01",
        "created=%3C2000-01-01",
        "created=%3E2999-01-01",
    ] {
        assert_eq!(
            request(
                &router,
                "GET",
                &format!("{base}/runs?per_page=20&exclude_pull_requests=true&{filter}"),
                &reader,
                None,
                200
            )
            .await["total_count"],
            0,
            "{filter}"
        );
    }
    request(
        &router,
        "GET",
        &format!("{base}/runs?head_sha=absent"),
        &reader,
        None,
        422,
    )
    .await;
    let (headers, bytes) = raw(
        &router,
        "GET",
        &format!("{base}/runs?per_page=1&exclude_pull_requests=true"),
        Some(&reader),
        None,
        200,
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["workflow_runs"][0]["id"],
        id
    );
    let next = headers["link"]
        .to_str()
        .unwrap()
        .split('>')
        .next()
        .unwrap()
        .trim_start_matches('<');
    let (_, bytes) = raw(&router, "GET", next, Some(&reader), None, 200).await;
    assert_ne!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["workflow_runs"][0]["id"],
        id
    );
    for token in [None, Some(outsider.as_str())] {
        raw(
            &router,
            "GET",
            &format!("{base}/runs?per_page=20&exclude_pull_requests=true"),
            token,
            None,
            404,
        )
        .await;
        raw(
            &router,
            "GET",
            &format!("{base}/workflows?per_page=100&page=1"),
            token,
            None,
            404,
        )
        .await;
    }
    request(
        &router,
        "POST",
        &format!("{base}/runs/{id}/cancel"),
        &reader,
        None,
        403,
    )
    .await;
    request(
        &router,
        "POST",
        &format!("{base}/runs/{id}/cancel"),
        &writer,
        None,
        202,
    )
    .await;
    run_fields(
        &request(
            &router,
            "GET",
            &format!("{base}/runs/{id}?exclude_pull_requests=true"),
            &reader,
            None,
            200,
        )
        .await,
        id,
        wid,
        sha,
        ("completed", json!("cancelled"), 1),
    );
    request(
        &router,
        "POST",
        &format!("{base}/runs/{id}/cancel"),
        &writer,
        None,
        409,
    )
    .await;
    assert_eq!(
        request(
            &router,
            "GET",
            &format!("{base}/runs?per_page=20&exclude_pull_requests=true&status=completed"),
            &reader,
            None,
            200
        )
        .await["total_count"],
        1
    );
    request(
        &router,
        "POST",
        &format!("{base}/runs/{id}/rerun"),
        &reader,
        None,
        403,
    )
    .await;
    request(
        &router,
        "POST",
        &format!("{base}/runs/{id}/rerun"),
        &writer,
        None,
        201,
    )
    .await;
    let reruns = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 10)
        .await
        .unwrap();
    assert_eq!(reruns.len(), 3);
    let rerun = reruns.iter().max_by_key(|run| run.id).unwrap();
    assert_eq!(rerun.head_sha.as_deref(), Some(sha));
    assert_eq!(
        rerun.workflow_inputs.as_ref().unwrap()["target"],
        "production"
    );
    assert_eq!(rerun.workflow_path, ".github/workflows/ci.yml");
    // The run list leaves out workflow snapshots, so compare the whole runs.
    let rerun_blobs = db::ci::get_ci_run(f.pool(), rerun.id)
        .await
        .unwrap()
        .unwrap()
        .workflow_blobs;
    let source_blobs = db::ci::get_ci_run(f.pool(), id)
        .await
        .unwrap()
        .unwrap()
        .workflow_blobs;
    assert!(rerun_blobs
        .as_object()
        .is_some_and(|blobs| !blobs.is_empty()));
    assert_eq!(rerun_blobs, source_blobs);

    // The lookup must filter before first:1, not select a newer unrelated branch.
    let mut matching = None;
    for head in ["feature", "unrelated"] {
        let pull = db::pull_requests::records::create_pull_request(
            f.pool(),
            db::pull_requests::records::NewPullRequest {
                repo_id: repo.id,
                head_repo_id: Some(repo.id),
                author_id: alice,
                issue: None,
                title: head,
                body: "",
                head_branch: head,
                head_sha: sha,
                base_branch: "main",
                base_sha: sha,
                is_draft: false,
                maintainer_can_modify: true,
            },
        )
        .await
        .unwrap();
        if head == "feature" {
            matching = Some(pull.number);
        }
    }
    let data = graphql(
        &router,
        &writer,
        PULL_REQUEST,
        json!({"owner":"alice","repo":"proj","headRefName":"feature"}),
    )
    .await;
    let nodes = data["repository"]["pullRequests"]["nodes"]
        .as_array()
        .unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["number"], matching.unwrap());
    assert_eq!(nodes[0]["headRepository"]["name"], "proj");
    assert_eq!(nodes[0]["headRepository"]["owner"]["login"], "alice");
    assert!(graphql(
        &router,
        &writer,
        PULL_REQUEST,
        json!({"owner":"alice","repo":"proj","headRefName":"absent"})
    )
    .await["repository"]["pullRequests"]["nodes"]
        .as_array()
        .unwrap()
        .is_empty());

    let secrets = format!("{base}/secrets");
    let key = request(
        &router,
        "GET",
        &format!("{secrets}/public-key"),
        &writer,
        None,
        200,
    )
    .await;
    assert!(!key["key_id"].as_str().unwrap().is_empty());
    assert_eq!(
        STANDARD.decode(key["key"].as_str().unwrap()).unwrap().len(),
        32
    );
    for expected in [201, 204] {
        request(
            &router,
            "PUT",
            &format!("{secrets}/TOKEN"),
            &writer,
            Some(sealed(&key, b"gh secret")),
            expected,
        )
        .await;
    }
    let listing = request(
        &router,
        "GET",
        &format!("{secrets}?per_page=100"),
        &writer,
        None,
        200,
    )
    .await;
    assert_eq!(listing["secrets"].as_array().unwrap().len(), 1);
    assert_eq!(listing["secrets"][0]["name"], "TOKEN");
    timestamp(&listing["secrets"][0]["updated_at"]);
    request(
        &router,
        "DELETE",
        &format!("{secrets}/TOKEN"),
        &writer,
        None,
        204,
    )
    .await;
    assert!(request(
        &router,
        "GET",
        &format!("{secrets}?per_page=100"),
        &writer,
        None,
        200
    )
    .await["secrets"]
        .as_array()
        .unwrap()
        .is_empty());
    let variables = format!("{base}/variables");
    request(
        &router,
        "POST",
        &variables,
        &writer,
        Some(json!({"name":"REGION","value":"west"})),
        201,
    )
    .await;
    request(
        &router,
        "POST",
        &variables,
        &writer,
        Some(json!({"name":"REGION","value":"east"})),
        409,
    )
    .await;
    assert_eq!(
        request(
            &router,
            "GET",
            &format!("{variables}/REGION"),
            &writer,
            None,
            200
        )
        .await["value"],
        "west"
    );
    request(
        &router,
        "PATCH",
        &format!("{variables}/REGION"),
        &writer,
        Some(json!({"value":"east"})),
        204,
    )
    .await;
    let variable = request(
        &router,
        "GET",
        &format!("{variables}/REGION"),
        &writer,
        None,
        200,
    )
    .await;
    assert_eq!(variable["name"], "REGION");
    assert_eq!(variable["value"], "east");
    timestamp(&variable["created_at"]);
    timestamp(&variable["updated_at"]);
    assert_eq!(
        request(
            &router,
            "GET",
            &format!("{variables}?per_page=100"),
            &writer,
            None,
            200
        )
        .await["variables"][0],
        variable
    );
    request(
        &router,
        "DELETE",
        &format!("{variables}/REGION"),
        &writer,
        None,
        204,
    )
    .await;
    request(
        &router,
        "GET",
        &format!("{variables}/REGION"),
        &writer,
        None,
        404,
    )
    .await;

    f.execute(
        "UPDATE ci_runs SET status='running',worker='test',started_at=created_at WHERE id=$1",
        &[rerun.id],
    )
    .await;
    // Credentials go only to the worker that holds the claim, so issue them for the claimed row.
    let runtime = db::ci::get_ci_run(f.pool(), rerun.id)
        .await
        .unwrap()
        .unwrap();
    let token = codeyard::backend::ci::runtime::issue(
        f.pool(),
        &runtime,
        &db::actions::ActionsSettings::default(),
    )
    .await
    .unwrap();
    let cache1 = results::cache(&router, &token.token, "deps-v1", b"one").await;
    let cache2 = results::cache(&router, &token.token, "deps-v2", b"two").await;
    let caches = request(
        &router,
        "GET",
        &format!("{base}/caches?per_page=30&sort=last_accessed_at&direction=desc"),
        &reader,
        None,
        200,
    )
    .await;
    assert_eq!(caches["total_count"], 2);
    for cache in caches["actions_caches"].as_array().unwrap() {
        assert!([cache1, cache2].contains(&cache["id"].as_i64().unwrap()));
        assert!(cache["key"].as_str().unwrap().starts_with("deps-v"));
        assert_eq!(cache["ref"], "refs/heads/main");
        assert_eq!(cache["size_in_bytes"], 3);
        assert!(!cache["version"].as_str().unwrap().is_empty());
        timestamp(&cache["created_at"]);
        timestamp(&cache["last_accessed_at"]);
    }
    let filtered = request(&router,"GET",&format!("{base}/caches?per_page=1&sort=size_in_bytes&direction=asc&key=deps-v1&ref=refs%2Fheads%2Fmain"),&reader,None,200).await;
    assert_eq!(filtered["total_count"], 1);
    assert_eq!(filtered["actions_caches"][0]["id"], cache1);
    assert_eq!(
        request(
            &router,
            "GET",
            &format!("{base}/caches?per_page=100&ref=refs%2Fheads%2Fmain"),
            &writer,
            None,
            200
        )
        .await["total_count"],
        2
    );
    let deleted = request(
        &router,
        "DELETE",
        &format!("{base}/caches?key=deps-v1&ref=refs%2Fheads%2Fmain"),
        &writer,
        None,
        200,
    )
    .await;
    assert_eq!(deleted["total_count"], 1);
    request(
        &router,
        "DELETE",
        &format!("{base}/caches/{cache2}"),
        &writer,
        None,
        204,
    )
    .await;
    let mut zip = vec![0; 22];
    zip[..4].copy_from_slice(b"PK\x05\x06");
    let artifact = actions_objects::artifact(&router, &runtime, &token.token, "dist", &zip).await;
    for path in [
        format!("{base}/artifacts?per_page=100"),
        format!("{base}/runs/{id}/artifacts?per_page=100"),
    ] {
        let listing = request(&router, "GET", &path, &reader, None, 200).await;
        assert_eq!(listing["artifacts"].as_array().unwrap().len(), 1);
        let entry = &listing["artifacts"][0];
        assert_eq!(entry["name"], "dist");
        assert_eq!(entry["workflow_run"]["id"], id);
        assert_eq!(entry["workflow_run"]["head_sha"], sha);
        assert_eq!(entry["size_in_bytes"], 22);
        assert_eq!(entry["expired"], false);
        assert!(entry["archive_download_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!(
                "/api/v3/repos/alice/proj/actions/artifacts/{artifact}/zip"
            )));
        let (headers, bytes) = raw(
            &router,
            "GET",
            entry["archive_download_url"].as_str().unwrap(),
            Some(&reader),
            None,
            302,
        )
        .await;
        assert!(bytes.is_empty());
        let (_, bytes) = raw(
            &router,
            "GET",
            headers["location"].to_str().unwrap(),
            None,
            None,
            200,
        )
        .await;
        assert_eq!(bytes, zip);
    }
    // A worker's running and finished states use the same vocabulary gh filters on.
    f.execute(
        "UPDATE ci_runs SET status='running',started_at=created_at WHERE repo_id=$1 AND status='queued'",
        &[repo.id],
    )
    .await;
    run_fields(
        &request(
            &router,
            "GET",
            &format!("{base}/runs/{id}?exclude_pull_requests=true"),
            &reader,
            None,
            200,
        )
        .await,
        id,
        wid,
        sha,
        ("in_progress", Value::Null, 2),
    );
    assert_eq!(
        request(
            &router,
            "GET",
            &format!("{base}/runs?per_page=20&exclude_pull_requests=true&status=in_progress"),
            &reader,
            None,
            200
        )
        .await["total_count"],
        2
    );
    f.execute(
        "UPDATE ci_runs SET status='failed',completed_at=updated_at WHERE id=$1",
        &[rerun.id],
    )
    .await;
    run_fields(
        &request(
            &router,
            "GET",
            &format!("{base}/runs/{id}?exclude_pull_requests=true"),
            &reader,
            None,
            200,
        )
        .await,
        id,
        wid,
        sha,
        ("completed", json!("failure"), 2),
    );
    assert_eq!(
        request(
            &router,
            "GET",
            &format!("{base}/runs?per_page=20&exclude_pull_requests=true&status=failure"),
            &reader,
            None,
            200
        )
        .await["total_count"],
        1
    );
    f.finish().await;
}

#[tokio::test]
async fn gh_apps_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn gh_apps_postgres() {
    contract(true).await;
}
