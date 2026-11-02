//! Toolkit-shaped Twirp requests and Azure block blobs over the real router on both engines.
use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard::backend::{
    ci::runtime,
    db::{self, action_objects as objects},
};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const CACHE: &str = "github.actions.results.api.v1.CacheService";
pub(super) const ARTIFACT: &str = "github.actions.results.api.v1.ArtifactService";

async fn request(
    router: &Router,
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Vec<u8>,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let url = url::Url::parse(url)
        .map(|u| {
            format!(
                "{}{}",
                u.path(),
                u.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        })
        .unwrap_or_else(|_| url.into());
    let mut request = Request::builder().method(method).uri(url);
    if matches!(method, "GET" | "HEAD") {
        request = request.header("accept-encoding", "gzip");
    }
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(range) = range {
        request = request.header("range", range);
    }
    if method == "POST" {
        request = request.header("content-type", "application/json");
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

pub(super) async fn rpc(
    router: &Router,
    token: &str,
    service: &str,
    method: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    let (status, _, bytes) = request(
        router,
        "POST",
        &format!("/twirp/{service}/{method}"),
        Some(token),
        serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await;
    assert_eq!(
        status,
        expected,
        "{method}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

pub(super) async fn run(
    f: &Database,
    repo: i64,
    actor: i64,
    branch: &str,
) -> (db::models::CiRun, runtime::RuntimeCredentials) {
    let sql="INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,pipeline,event,branch,workflow_path,status,worker) VALUES($1,$2,$3,'local','ci','push',$4,'.github/workflows/ci.yml','running','test') RETURNING *";
    let row = match f.pool() {
        db::DbPool::Postgres(pool) => sqlx::query_as(sql)
            .bind(Uuid::new_v4())
            .bind(repo)
            .bind(actor)
            .bind(branch)
            .fetch_one(pool)
            .await
            .unwrap(),
        db::DbPool::Sqlite(pool) => sqlx::query_as(sql)
            .bind(Uuid::new_v4())
            .bind(repo)
            .bind(actor)
            .bind(branch)
            .fetch_one(pool)
            .await
            .unwrap(),
    };
    let credentials = runtime::issue(f.pool(), &row, &db::actions::ActionsSettings::default())
        .await
        .unwrap();
    (row, credentials)
}

pub(super) fn artifact_body(run: &db::models::CiRun, name: &str) -> Value {
    json!({"workflowRunBackendId":run.run_id.to_string(),"workflowJobRunBackendId":run.run_id.to_string(),"name":name})
}
pub(super) async fn put(router: &Router, url: &str, body: &[u8], expected: StatusCode) {
    let (status, _, bytes) = request(router, "PUT", url, None, body.to_vec(), None).await;
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(&bytes));
}
pub(super) async fn cache(router: &Router, token: &str, key: &str, bytes: &[u8]) -> i64 {
    let body = json!({"key":key,"version":"tar-zstd-v1"});
    let created = rpc(
        router,
        token,
        CACHE,
        "CreateCacheEntry",
        body.clone(),
        StatusCode::OK,
    )
    .await;
    put(
        router,
        created["signedUploadUrl"].as_str().unwrap(),
        bytes,
        StatusCode::CREATED,
    )
    .await;
    let mut finalized = body;
    finalized["sizeBytes"] = json!(bytes.len().to_string());
    rpc(
        router,
        token,
        CACHE,
        "FinalizeCacheEntryUpload",
        finalized,
        StatusCode::OK,
    )
    .await["entryId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}
async fn restore(router: &Router, token: &str, key: &str, keys: &[&str]) -> Value {
    rpc(
        router,
        token,
        CACHE,
        "GetCacheEntryDownloadURL",
        json!({"key":key,"restoreKeys":keys,"version":"tar-zstd-v1"}),
        StatusCode::OK,
    )
    .await
}

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("results_alice").await;
    let bob = f.actor("results_bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "results",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let (main, main_token) = run(&f, repo.id, alice, "main").await;
    let (feature, feature_token) = run(&f, repo.id, alice, "feature").await;
    let (_, other_token) = run(&f, repo.id, alice, "other").await;
    let other_repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "separate",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let (_, foreign_token) = run(&f, other_repo.id, alice, "main").await;
    let token = &main_token.token;
    // This is the JWT claim the artifact toolkit reads before making its first call.
    use base64::Engine;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1).unwrap())
        .unwrap();
    let claims: Value = serde_json::from_slice(&claims).unwrap();
    assert_eq!(
        claims["scp"],
        format!("Actions.Results:{}:{}", main.run_id, main.run_id)
    );
    rpc(
        &router,
        "invalid",
        CACHE,
        "CreateCacheEntry",
        json!({"key":"bad","version":"v"}),
        StatusCode::FORBIDDEN,
    )
    .await;

    // Blocks may arrive out of order, and retries replace a block rather than append it.
    let mut create = artifact_body(&main, "build");
    create["version"] = json!(4);
    let created = rpc(
        &router,
        token,
        ARTIFACT,
        "CreateArtifact",
        create.clone(),
        StatusCode::OK,
    )
    .await;
    let upload = created["signedUploadUrl"].as_str().unwrap();
    assert_eq!(
        request(&router, "GET", upload, None, vec![], None).await.0,
        StatusCode::FORBIDDEN
    );
    let zip = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
    put(
        &router,
        &format!("{upload}&comp=block&blockid=Yg%3D%3D"),
        &zip[8..],
        StatusCode::CREATED,
    )
    .await;
    put(
        &router,
        &format!("{upload}&comp=block&blockid=YQ%3D%3D"),
        b"retry",
        StatusCode::CREATED,
    )
    .await;
    put(
        &router,
        &format!("{upload}&comp=block&blockid=YQ%3D%3D"),
        &zip[..8],
        StatusCode::CREATED,
    )
    .await;
    // Finalization before the block list, and a block list with missing IDs, are refused.
    let mut finalize = artifact_body(&main, "build");
    finalize["size"] = json!(zip.len().to_string());
    rpc(
        &router,
        token,
        ARTIFACT,
        "FinalizeArtifact",
        finalize.clone(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    put(
        &router,
        &format!("{upload}&comp=blocklist"),
        b"<BlockList><Latest>eA==</Latest></BlockList>",
        StatusCode::BAD_REQUEST,
    )
    .await;
    put(&router,&format!("{upload}&comp=blocklist"),b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><BlockList><Latest>YQ==</Latest><Latest>Yg==</Latest></BlockList>",StatusCode::CREATED).await;
    let mut wrong = finalize.clone();
    wrong["size"] = json!("1");
    rpc(
        &router,
        token,
        ARTIFACT,
        "FinalizeArtifact",
        wrong,
        StatusCode::BAD_REQUEST,
    )
    .await;
    let finalized = rpc(
        &router,
        token,
        ARTIFACT,
        "FinalizeArtifact",
        finalize.clone(),
        StatusCode::OK,
    )
    .await;
    let artifact_id: i64 = finalized["artifactId"].as_str().unwrap().parse().unwrap();
    assert!((90 * 86400 - 5..=90 * 86400).contains(
        &f.scalar(
            "SELECT expires_at-created_at FROM actions_objects WHERE id=$1",
            &[artifact_id]
        )
        .await
    ));
    rpc(
        &router,
        token,
        ARTIFACT,
        "FinalizeArtifact",
        finalize.clone(),
        StatusCode::OK,
    )
    .await;
    put(
        &router,
        &format!("{upload}&comp=block&blockid=YQ%3D%3D"),
        b"mutated",
        StatusCode::BAD_REQUEST,
    )
    .await;
    let listed = rpc(
        &router,
        token,
        ARTIFACT,
        "ListArtifacts",
        artifact_body(&main, ""),
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(listed["artifacts"][0]["size"], zip.len().to_string());
    let mut filtered = artifact_body(&main, "");
    filtered["idFilter"] = json!(artifact_id.to_string());
    assert_eq!(
        rpc(
            &router,
            token,
            ARTIFACT,
            "ListArtifacts",
            filtered.clone(),
            StatusCode::OK
        )
        .await["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    filtered["nameFilter"] = json!("missing");
    assert!(rpc(
        &router,
        token,
        ARTIFACT,
        "ListArtifacts",
        filtered,
        StatusCode::OK
    )
    .await["artifacts"]
        .as_array()
        .unwrap()
        .is_empty());
    // A valid credential for another run cannot list, finalize, delete or mint URLs for this run.
    for method in [
        "ListArtifacts",
        "FinalizeArtifact",
        "DeleteArtifact",
        "GetSignedArtifactURL",
    ] {
        rpc(
            &router,
            &feature_token.token,
            ARTIFACT,
            method,
            finalize.clone(),
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    let download = rpc(
        &router,
        token,
        ARTIFACT,
        "GetSignedArtifactURL",
        artifact_body(&main, "build"),
        StatusCode::OK,
    )
    .await;
    let url = download["signedUrl"].as_str().unwrap();
    put(&router, url, b"overwrite", StatusCode::FORBIDDEN).await;
    let (status, _, bytes) = request(&router, "GET", url, None, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, zip);
    let (status, headers, bytes) = request(&router, "HEAD", url, None, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-length"], zip.len().to_string());
    assert!(bytes.is_empty());
    let (status, headers, bytes) =
        request(&router, "GET", url, None, vec![], Some("bytes=5-12")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes, zip[5..13]);
    assert_eq!(
        headers["content-range"],
        format!("bytes 5-12/{}", zip.len())
    );
    assert_eq!(
        request(&router, "GET", url, None, vec![], Some("bytes=999-"))
            .await
            .0,
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    assert_eq!(
        request(
            &router,
            "GET",
            &url.replace("sig=", "sig=invalid"),
            None,
            vec![],
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    let owner = Client::login(&router, f.pool(), alice).await;
    let stranger = Client::login(&router, f.pool(), bob).await;
    let run_page = format!("/results_alice/results/actions?job={}", main.run_id);
    let page = owner.page_ok(&run_page).await;
    assert!(page.contains("build.zip"), "{page}");
    let path = format!(
        "/results_alice/results/actions/runs/{}/artifacts/{artifact_id}",
        main.run_id
    );
    assert_eq!(
        stranger
            .form_reply(&format!("{path}/delete"), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .form_reply(&format!("{path}/delete"), &[])
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        request(&router, "GET", url, None, vec![], None).await.0,
        StatusCode::FORBIDDEN
    );

    // The cache SDK sends archives up to 128 MiB as one PUT, larger than a file-store object.
    let mut large = vec![b'x'; 101 * 1024 * 1024];
    let boundary = 64 * 1024 * 1024;
    large[boundary - 2..boundary + 2].copy_from_slice(b"test");
    let large_id = cache(&router, token, "large-single-shot", &large).await;
    drop(large);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_blob_parts WHERE object_id=$1",
            &[large_id]
        )
        .await,
        2
    );
    let large_url = restore(&router, token, "large-single-shot", &[]).await;
    assert_eq!(
        request(
            &router,
            "GET",
            large_url["signedDownloadUrl"].as_str().unwrap(),
            None,
            vec![],
            Some(&format!("bytes={}-{}", boundary - 2, boundary + 1))
        )
        .await
        .2,
        b"test"
    );

    let first = cache(&router, token, "linux-deps-1", b"old-cache").await;
    cache(&router, token, "linux-deps-2", b"new-cache").await;
    assert_eq!(
        restore(&router, &foreign_token.token, "linux-deps-1", &[]).await["ok"],
        false
    );
    assert_eq!(
        rpc(
            &router,
            token,
            CACHE,
            "GetCacheEntryDownloadURL",
            json!({"key":"linux-deps-1","version":"different-compression"}),
            StatusCode::OK
        )
        .await["ok"],
        false
    );
    let restored = restore(&router, &feature_token.token, "linux-deps-1", &[]).await;
    assert_eq!(restored["matchedKey"], "linux-deps-1");
    let restored = restore(&router, &feature_token.token, "absent", &["linux-deps-"]).await;
    assert_eq!(restored["matchedKey"], "linux-deps-2");
    assert_eq!(
        request(
            &router,
            "GET",
            restored["signedDownloadUrl"].as_str().unwrap(),
            None,
            vec![],
            None
        )
        .await
        .2,
        b"new-cache"
    );
    cache(
        &router,
        &feature_token.token,
        "linux-deps-feature",
        b"branch-cache",
    )
    .await;
    assert_eq!(
        restore(&router, &feature_token.token, "absent", &["linux-deps-"]).await["matchedKey"],
        "linux-deps-feature"
    );
    assert_eq!(
        restore(
            &router,
            &feature_token.token,
            "linux-deps-2",
            &["linux-deps-"]
        )
        .await["matchedKey"],
        "linux-deps-feature"
    );
    assert_ne!(
        restore(&router, &other_token.token, "absent", &["linux-deps-"]).await["matchedKey"],
        "linux-deps-feature"
    );
    rpc(
        &router,
        token,
        CACHE,
        "CreateCacheEntry",
        json!({"key":"linux-deps-1","version":"tar-zstd-v1"}),
        StatusCode::CONFLICT,
    )
    .await;
    rpc(
        &router,
        &feature_token.token,
        CACHE,
        "FinalizeCacheEntryUpload",
        json!({"key":"linux-deps-1","version":"tar-zstd-v1","sizeBytes":"9"}),
        StatusCode::NOT_FOUND,
    )
    .await;

    // Expiry is enforced on lookup and signed capabilities before the bounded sweep removes bytes.
    f.execute(
        "UPDATE actions_objects SET expires_at=0 WHERE id=$1",
        &[first],
    )
    .await;
    assert_eq!(
        restore(&router, token, "linux-deps-1", &[]).await["ok"],
        false
    );
    cache(&router, token, "linux-deps-1", b"replacement").await;
    objects::expire_objects(f.pool()).await.unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM actions_objects WHERE id=$1", &[first])
            .await,
        0
    );
    assert!(
        f.scalar("SELECT COUNT(*) FROM pending_object_deletions", &[])
            .await
            >= 2
    );

    // Fork run tokens can restore default-branch caches, never publish into the base's scope.
    f.execute("UPDATE ci_runs SET is_fork=TRUE WHERE id=$1", &[feature.id])
        .await;
    rpc(
        &router,
        &feature_token.token,
        CACHE,
        "CreateCacheEntry",
        json!({"key":"fork-write","version":"v"}),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(
        restore(&router, &feature_token.token, "linux-deps-2", &[]).await["ok"],
        true
    );

    let mut short = artifact_body(&feature, "short-retention");
    short["version"] = json!(4);
    short["expiresAt"] = json!((chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339());
    let short_upload = rpc(
        &router,
        &feature_token.token,
        ARTIFACT,
        "CreateArtifact",
        short.clone(),
        StatusCode::OK,
    )
    .await;
    // A single-shot upload whose response was lost can be sent again before finalizing.
    for _ in 0..2 {
        put(
            &router,
            short_upload["signedUploadUrl"].as_str().unwrap(),
            zip,
            StatusCode::CREATED,
        )
        .await;
    }
    short["size"] = json!(zip.len().to_string());
    let short_id: i64 = rpc(
        &router,
        &feature_token.token,
        ARTIFACT,
        "FinalizeArtifact",
        short,
        StatusCode::OK,
    )
    .await["artifactId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        f.scalar(
            "SELECT expires_at-created_at FROM actions_objects WHERE id=$1",
            &[short_id]
        )
        .await
            <= 86400
    );

    if pg {
        f.execute(
            "UPDATE ci_runs SET status='success',completed_at=NOW() WHERE id=$1",
            &[main.id],
        )
        .await;
    } else {
        f.execute("UPDATE ci_runs SET status='success',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=$1", &[main.id]).await;
    }
    assert_eq!(
        restore(&router, token, "linux-deps-2", &[]).await["ok"],
        true
    );
    // The run's five-minute post-completion grace ends; its old bearer credential then fails.
    if pg {
        f.execute("UPDATE ci_runs SET status='success',completed_at=NOW()-INTERVAL '6 minutes' WHERE id=$1",&[main.id]).await;
    } else {
        f.execute("UPDATE ci_runs SET status='success',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now','-6 minutes') WHERE id=$1",&[main.id]).await;
    }
    rpc(
        &router,
        token,
        CACHE,
        "GetCacheEntryDownloadURL",
        json!({"key":"linux-deps-2","version":"tar-zstd-v1"}),
        StatusCode::FORBIDDEN,
    )
    .await;

    // Finalized caches outlive their creating run; artifacts and unfinished uploads do not.
    f.execute("DELETE FROM ci_runs WHERE id=$1", &[main.id])
        .await;
    assert_eq!(
        restore(&router, &other_token.token, "linux-deps-2", &[]).await["ok"],
        true
    );
    // The run/repository foreign keys and part-deletion trigger cover lifecycle cleanup.
    f.execute("DELETE FROM ci_runs WHERE id=$1", &[feature.id])
        .await;
    assert!(objects::artifact_by_id(f.pool(), short_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_objects WHERE run_id=$1",
            &[feature.id]
        )
        .await,
        0
    );
    f.execute("DELETE FROM repositories WHERE id=$1", &[repo.id])
        .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM actions_objects WHERE repo_id=$1",
            &[repo.id]
        )
        .await,
        0
    );
    f.execute(
        "UPDATE pending_object_deletions SET delete_after='2000-01-01T00:00:00.000000Z'",
        &[],
    )
    .await;
    let storage = codeyard::backend::storage::ObjectStore::open(root.path(), None)
        .await
        .unwrap();
    codeyard::backend::files::remove_expired_uploads(f.pool(), &storage)
        .await
        .unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM pending_object_deletions", &[])
            .await,
        0
    );
    let mut objects = tokio::fs::read_dir(root.path().join("objects/actions"))
        .await
        .unwrap();
    assert!(objects.next_entry().await.unwrap().is_none());
    f.finish().await;
}

async fn eviction(pg: bool) {
    let mut f = Database::new(pg).await;
    let alice = f.actor("eviction_alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "eviction",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let (run, token) = run(&f, repo.id, alice, "main").await;
    let a = cache(&router, &token.token, "a", b"1234").await;
    let b = cache(&router, &token.token, "b", b"5678").await;
    f.execute("UPDATE actions_objects SET accessed_at=accessed_at-20,expires_at=expires_at-20 WHERE id=$1", &[a]).await;
    f.execute("UPDATE actions_objects SET accessed_at=accessed_at-10,expires_at=expires_at-10 WHERE id=$1", &[b]).await;
    assert_eq!(restore(&router, &token.token, "a", &[]).await["ok"], true);
    // Exercise the same finalization transaction with a byte-sized quota, avoiding GiB fixtures.
    assert!(objects::finalize_object(f.pool(), run.id, b, 4, 6)
        .await
        .unwrap());
    assert_eq!(restore(&router, &token.token, "a", &[]).await["ok"], true);
    assert_eq!(restore(&router, &token.token, "b", &[]).await["ok"], false);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM pending_object_deletions", &[])
            .await,
        1
    );
    assert!(!objects::finalize_object(f.pool(), run.id, a, 4, 3)
        .await
        .unwrap());
    f.finish().await;
}

async fn pending_uploads_share_the_cache_quota(pg: bool) {
    let mut f = Database::new(pg).await;
    let alice = f.actor("pending_quota_alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "pending-quota",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let (_run, token) = run(&f, repo.id, alice, "main").await;
    let auth = objects::authenticate(f.pool(), &runtime::token_hash(&token.token))
        .await
        .unwrap()
        .unwrap();
    let expires_at = chrono::Utc::now().timestamp() + 86400;
    let a = objects::create_object(
        f.pool(),
        objects::NewObject {
            run: &auth,
            kind: "cache",
            name: "a",
            version: "v1",
            expires_at,
        },
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    // A retried create gets this run's own unfinished reservation back.
    let retried = objects::create_object(
        f.pool(),
        objects::NewObject {
            run: &auth,
            kind: "cache",
            name: "a",
            version: "v1",
            expires_at,
        },
    )
    .await
    .unwrap();
    assert_eq!(retried.map(|object| object.id), Some(a));
    let b = objects::create_object(
        f.pool(),
        objects::NewObject {
            run: &auth,
            kind: "cache",
            name: "b",
            version: "v1",
            expires_at,
        },
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    let part = |size: i64| objects::BlobPart {
        block_id: "plain-0".into(),
        object_key: format!("actions/{}", Uuid::new_v4()),
        size,
    };
    // Four bytes of `a` are in flight, so `b` cannot stage four more under a six-byte quota.
    assert!(objects::stage_block(f.pool(), a, &part(4), 6)
        .await
        .unwrap());
    assert!(!objects::stage_block(f.pool(), b, &part(4), 6)
        .await
        .unwrap());
    assert!(objects::stage_block(f.pool(), b, &part(2), 6)
        .await
        .unwrap());
    f.finish().await;
}

#[tokio::test]
async fn sqlite_results_pending_uploads_share_quota() {
    pending_uploads_share_the_cache_quota(false).await;
}
#[tokio::test]
async fn postgres_results_pending_uploads_share_quota() {
    pending_uploads_share_the_cache_quota(true).await;
}

#[tokio::test]
async fn sqlite_results_protocol() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_results_protocol() {
    contract(true).await;
}
#[tokio::test]
async fn sqlite_results_lru_eviction() {
    eviction(false).await;
}
#[tokio::test]
async fn postgres_results_lru_eviction() {
    eviction(true).await;
}
