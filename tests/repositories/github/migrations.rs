use super::*;
use axum::{
    http::header,
    response::IntoResponse,
    routing::{get, post},
};
use codeyard::backend::repositories::{lifecycle, store::repo_store};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

mod exports;

fn packet(bytes: &[u8], output: &mut Vec<u8>) {
    output.extend_from_slice(format!("{:04x}", bytes.len() + 4).as_bytes());
    output.extend_from_slice(bytes);
}

struct Source {
    url: String,
    tip: String,
    tag: String,
    requests: Arc<AtomicUsize>,
    blocked: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn source() -> Source {
    let directory = tempfile::tempdir().unwrap();
    let git = git2::Repository::init_bare(directory.path()).unwrap();
    let blob = git.blob(b"imported\n").unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    tree.insert("README.md", blob, 0o100644).unwrap();
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Original author", "original@example.test").unwrap();
    let tip = git
        .commit(
            Some("refs/heads/trunk"),
            &signature,
            &signature,
            "Original commit",
            &tree,
            &[],
        )
        .unwrap();
    git.set_head("refs/heads/trunk").unwrap();
    let tag = git
        .tag(
            "v1",
            &git.find_object(tip, None).unwrap(),
            &signature,
            "Original tag",
            false,
        )
        .unwrap();
    let mut pack = git.packbuilder().unwrap();
    pack.insert_commit(tip).unwrap();
    pack.insert_object(tag, None).unwrap();
    let mut buffer = git2::Buf::new();
    pack.write_buf(&mut buffer).unwrap();
    let pack = buffer.to_vec();
    let mut advertisement = Vec::new();
    packet(b"# service=git-upload-pack\n", &mut advertisement);
    advertisement.extend_from_slice(b"0000");
    packet(
        format!("{tip} HEAD\0side-band-64k ofs-delta symref=HEAD:refs/heads/trunk\n").as_bytes(),
        &mut advertisement,
    );
    for (name, sha) in [
        ("refs/heads/trunk", tip),
        ("refs/tags/v1", tag),
        ("refs/pull/7/head", tip),
        ("refs/codeyard", tip),
        ("refs/codeyard/publications/untrusted", tip),
    ] {
        packet(format!("{sha} {name}\n").as_bytes(), &mut advertisement);
    }
    advertisement.extend_from_slice(b"0000");
    let mut transfer = Vec::new();
    packet(b"NAK\n", &mut transfer);
    for chunk in pack.chunks(60000) {
        let mut sideband = vec![1];
        sideband.extend_from_slice(chunk);
        packet(&sideband, &mut transfer);
    }
    transfer.extend_from_slice(b"0000");
    let requests = Arc::new(AtomicUsize::new(0));
    let blocked = Arc::new(AtomicBool::new(false));
    let seen = requests.clone();
    let hold = blocked.clone();
    let app = Router::new()
        .route(
            "/source.git/info/refs",
            get(move |headers: HeaderMap| {
                let advertisement = advertisement.clone();
                let seen = seen.clone();
                let hold = hold.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    while hold.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    if headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        != Some("Basic dXBzdHJlYW06c2VjcmV0")
                    {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    (
                        [(
                            header::CONTENT_TYPE,
                            "application/x-git-upload-pack-advertisement",
                        )],
                        advertisement,
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/source.git/git-upload-pack",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let transfer = transfer.clone();
                async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Basic dXBzdHJlYW06c2VjcmV0");
                    // Imports ask for sideband only, not offset deltas, since 872d32a69.
                    let request = String::from_utf8_lossy(&body);
                    assert!(request.contains(" side-band-64k\n") && !request.contains("ofs-delta"));
                    (
                        [(header::CONTENT_TYPE, "application/x-git-upload-pack-result")],
                        transfer,
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/source.git", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Source {
        url,
        tip: tip.to_string(),
        tag: tag.to_string(),
        requests,
        blocked,
        task,
    }
}

async fn call(
    router: &Router,
    method: &str,
    base: &str,
    token: Option<&str>,
    input: Value,
    operation: &str,
    expected: StatusCode,
) -> Value {
    let (status, _, body) = request(router, method, base, token, input).await;
    assert_eq!(status, expected, "{operation}: {body}");
    if status != StatusCode::NO_CONTENT {
        assert_github_schema(operation, status.as_u16(), &body);
    }
    body
}

async fn wait_status(router: &Router, base: &str, token: &str, expected: &str) {
    // An import runs a fetch, index-pack and several ref writes in the background; a loaded
    // test machine can take longer than a few seconds for them.
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let body = call(
                router,
                "GET",
                base,
                Some(token),
                Value::Null,
                "migrations/get-import-status",
                StatusCode::OK,
            )
            .await;
            if body["status"] == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

async fn contract(pg: bool, source: &Source) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("import-owner").await;
    let bob = f.actor("import-outsider").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "destination",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let router = create_router(state);
    let session = Client::login(&router, f.pool(), alice).await;
    let other = Client::login(&router, f.pool(), bob).await;
    let mut token_values = Vec::new();
    for (actor, session, scope) in [
        (alice, &session, "write"),
        (alice, &session, "read"),
        (bob, &other, "write"),
    ] {
        token_values.push(format!(
            "Bearer {}",
            tokens::create(
                f.pool(),
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
        ));
    }
    let (writer, reader, outsider) = (&token_values[0], &token_values[1], &token_values[2]);
    let base = "/api/v3/repos/import-owner/destination/import";
    call(
        &router,
        "GET",
        base,
        Some(writer),
        Value::Null,
        "migrations/get-import-status",
        StatusCode::NOT_FOUND,
    )
    .await;
    call(
        &router,
        "GET",
        base,
        Some(outsider),
        Value::Null,
        "migrations/get-import-status",
        StatusCode::NOT_FOUND,
    )
    .await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    for (method, operation) in [
        ("GET", "migrations/get-commit-authors"),
        ("PATCH", "migrations/map-commit-author"),
    ] {
        let path = if method == "GET" {
            format!("{base}/authors")
        } else {
            format!("{base}/authors/1")
        };
        call(
            &router,
            method,
            &path,
            Some(outsider),
            json!({}),
            operation,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    for (method, operation) in [
        ("GET", "migrations/get-import-status"),
        ("PUT", "migrations/start-import"),
        ("PATCH", "migrations/update-import"),
        ("DELETE", "migrations/cancel-import"),
    ] {
        call(
            &router,
            method,
            base,
            Some(outsider),
            json!({"vcs_url": source.url}),
            operation,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    db::repositories::access::revoke_repo_access(f.pool(), bob, repo.id)
        .await
        .unwrap();
    for (method, operation) in [
        ("PUT", "migrations/start-import"),
        ("PATCH", "migrations/update-import"),
        ("DELETE", "migrations/cancel-import"),
    ] {
        call(
            &router,
            method,
            base,
            Some(reader),
            json!({"vcs_url": source.url}),
            operation,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    for input in [
        json!({}),
        json!({"vcs_url":"file:///etc/passwd"}),
        json!({"vcs_url":"https://name:secret@example.com/repo.git"}),
        json!({"vcs_url":source.url,"vcs":"subversion"}),
    ] {
        let body = call(
            &router,
            "PUT",
            base,
            Some(writer),
            input,
            "migrations/start-import",
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert!(body["errors"].is_array());
    }
    let body = call(
        &router,
        "PUT",
        base,
        Some(writer),
        json!({"vcs_url": source.url}),
        "migrations/start-import",
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(body["status"], "importing");
    assert!(!body.to_string().contains("secret"));
    wait_status(&router, base, reader, "error").await;
    call(
        &router,
        "PATCH",
        base,
        Some(writer),
        json!({"vcs_username":"upstream", "vcs_password":"secret"}),
        "migrations/update-import",
        StatusCode::OK,
    )
    .await;
    wait_status(&router, base, reader, "complete").await;
    // Commit authors map to accounts only through verified addresses, as on GitHub.
    f.execute(
        "UPDATE users SET email='original@example.test',email_verified=TRUE WHERE id=$1",
        &[alice],
    )
    .await;
    f.execute(
        "UPDATE users SET email='mapped@example.test',email_verified=TRUE WHERE id=$1",
        &[bob],
    )
    .await;
    let authors_path = format!("{base}/authors");
    let authors = call(
        &router,
        "GET",
        &authors_path,
        Some(reader),
        Value::Null,
        "migrations/get-commit-authors",
        StatusCode::OK,
    )
    .await;
    assert_eq!(authors.as_array().unwrap().len(), 1);
    assert_eq!(authors[0]["remote_name"], "Original author");
    assert_eq!(authors[0]["remote_id"], "original@example.test");
    assert_eq!(authors[0]["user"]["id"], alice);
    let author_id = authors[0]["id"].as_i64().unwrap();
    let author_path = format!("{authors_path}/{author_id}");
    call(
        &router,
        "PATCH",
        &author_path,
        Some(reader),
        json!({"name":"Denied"}),
        "migrations/map-commit-author",
        StatusCode::FORBIDDEN,
    )
    .await;
    let mapped = call(
        &router,
        "PATCH",
        &author_path,
        Some(writer),
        json!({"name":"Mapped author","email":"mapped@example.test"}),
        "migrations/map-commit-author",
        StatusCode::OK,
    )
    .await;
    assert_eq!(mapped["name"], "Mapped author");
    assert_eq!(mapped["email"], "mapped@example.test");
    assert_eq!(mapped["user"]["id"], bob);
    let authors = call(
        &router,
        "GET",
        &authors_path,
        Some(reader),
        Value::Null,
        "migrations/get-commit-authors",
        StatusCode::OK,
    )
    .await;
    assert_eq!(authors[0], mapped);
    let after = call(
        &router,
        "GET",
        &format!("{authors_path}?since={author_id}"),
        Some(reader),
        Value::Null,
        "migrations/get-commit-authors",
        StatusCode::OK,
    )
    .await;
    assert_eq!(after, json!([]));
    let store = repo_store(&repo, None);
    let refs = store.mirror_refs().await.unwrap();
    assert_eq!(refs.len(), 4); // Three imported refs and one locally attested branch proof.
    assert!(!refs.contains_key("refs/codeyard"));
    assert!(!refs.contains_key("refs/codeyard/publications/untrusted"));
    let proof = store
        .branch_publication(repo.id, "refs/heads/trunk")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(proof.publication.repository_id, repo.id);
    assert_eq!(proof.publication.actor.user_id(), Some(alice));
    assert_eq!(proof.publication.new, source.tip);
    assert_eq!(proof.publication.reference, "refs/heads/trunk");
    assert_eq!(proof.publication.old, "0".repeat(40));
    assert!(refs.values().any(|oid| oid == &proof.object_id));
    assert_eq!(refs["refs/heads/trunk"], source.tip);
    assert_eq!(refs["refs/tags/v1"], source.tag);
    assert_eq!(refs["refs/pull/7/head"], source.tip);
    assert_eq!(
        store.git_tag(&source.tag).await.unwrap().message,
        "Original tag"
    );
    assert_eq!(
        store.head_target().await.unwrap().as_deref(),
        Some("refs/heads/trunk")
    );
    assert_eq!(
        db::repositories::records::get_repository_by_id(f.pool(), repo.id)
            .await
            .unwrap()
            .unwrap()
            .default_branch,
        "trunk"
    );
    call(
        &router,
        "PUT",
        base,
        Some(writer),
        json!({"vcs_url":source.url}),
        "migrations/start-import",
        StatusCode::CONFLICT,
    )
    .await;
    call(
        &router,
        "DELETE",
        base,
        Some(writer),
        Value::Null,
        "migrations/cancel-import",
        StatusCode::NO_CONTENT,
    )
    .await;
    call(
        &router,
        "GET",
        base,
        Some(writer),
        Value::Null,
        "migrations/get-import-status",
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(store.mirror_refs().await.unwrap(), refs);
    exports::run(
        &router,
        &f,
        root.path(),
        (alice, bob),
        (writer, reader, outsider),
        &repo,
    )
    .await;

    // Cancellation wins before publication; a stale worker cannot publish into the next attempt.
    let empty = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "cancelled",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let base = "/api/v3/repos/import-owner/cancelled/import";
    source.blocked.store(true, Ordering::SeqCst);
    let seen = source.requests.load(Ordering::SeqCst);
    let input = json!({"vcs_url":source.url, "vcs_username":"upstream", "vcs_password":"secret"});
    call(
        &router,
        "PUT",
        base,
        Some(writer),
        input.clone(),
        "migrations/start-import",
        StatusCode::CREATED,
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while source.requests.load(Ordering::SeqCst) == seen {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    call(
        &router,
        "PATCH",
        base,
        Some(writer),
        json!({}),
        "migrations/update-import",
        StatusCode::CONFLICT,
    )
    .await;
    call(
        &router,
        "DELETE",
        base,
        Some(writer),
        Value::Null,
        "migrations/cancel-import",
        StatusCode::NO_CONTENT,
    )
    .await;
    source.blocked.store(false, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(repo_store(&empty, None)
        .mirror_refs()
        .await
        .unwrap()
        .is_empty());
    call(
        &router,
        "PUT",
        base,
        Some(writer),
        input,
        "migrations/start-import",
        StatusCode::CREATED,
    )
    .await;
    wait_status(&router, base, reader, "complete").await;

    let revoked = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "revoked",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let token = tokens::create(
        f.pool(),
        alice,
        Some(&session.session),
        TokenInput {
            name: "revoked import",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let base = "/api/v3/repos/import-owner/revoked/import";
    source.blocked.store(true, Ordering::SeqCst);
    let seen = source.requests.load(Ordering::SeqCst);
    call(
        &router,
        "PUT",
        base,
        Some(&format!("Bearer {}", token.value)),
        json!({"vcs_url":source.url, "vcs_username":"upstream", "vcs_password":"secret"}),
        "migrations/start-import",
        StatusCode::CREATED,
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while source.requests.load(Ordering::SeqCst) == seen {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokens::delete(f.pool(), alice, Some(&session.session), token.token.id)
        .await
        .unwrap();
    source.blocked.store(false, Ordering::SeqCst);
    wait_status(&router, base, reader, "error").await;
    assert!(repo_store(&revoked, None)
        .mirror_refs()
        .await
        .unwrap()
        .is_empty());

    let (first, second) = tokio::join!(
        db::source_imports::start(f.pool(), alice, revoked.id, &source.url, "first"),
        db::source_imports::start(f.pool(), alice, revoked.id, &source.url, "second"),
    );
    assert_ne!(
        first.is_ok(),
        second.is_ok(),
        "Only one concurrent attempt may be admitted"
    );
    let current = db::source_imports::get(f.pool(), revoked.id)
        .await
        .unwrap()
        .unwrap();
    db::source_imports::finish(f.pool(), revoked.id, "stale", true)
        .await
        .unwrap();
    assert_eq!(
        db::source_imports::get(f.pool(), revoked.id)
            .await
            .unwrap()
            .unwrap()
            .generation,
        current.generation
    );
    assert_eq!(
        db::source_imports::get(f.pool(), revoked.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "importing"
    );
    db::source_imports::cancel(f.pool(), alice, revoked.id)
        .await
        .unwrap();

    let html = session.page_ok("/new/migration").await;
    assert!(html.contains("New migration"));
    assert!(html.contains("name=\"source_token\""));
    assert!(html.contains("action=\"/new/migration\""));
    let before = f.count().await;
    let (status, _) = session
        .send(
            "POST",
            "/new/migration",
            "application/x-www-form-urlencoded",
            "name=unprotected".to_owned(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.count().await, before);
    let (status, html) = session
        .form(
            "/new/migration",
            &[
                ("name", "bad-source"),
                (
                    "source_url",
                    "https://name:do-not-leak@example.com/repo.git",
                ),
                ("source_token", "do-not-leak"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{html}");
    assert!(!html.contains("do-not-leak"));
    assert_eq!(f.count().await, before);

    source.blocked.store(true, Ordering::SeqCst);
    let reply = session
        .form_reply(
            "/new/migration",
            &[
                ("name", "browser"),
                ("private", "on"),
                ("source_url", &source.url),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert_eq!(reply.location.as_deref(), Some("/import-owner/browser"));
    let html = session.page_ok("/import-owner/browser").await;
    assert!(html.contains("Migration in progress"));
    assert!(html.contains("Manage migration"));
    let html = session.page_ok("/import-owner/browser/migration").await;
    assert!(html.contains("Repository migration"));
    assert!(!html.contains("do-not-leak"));
    let browser = db::repositories::records::get_repository(f.pool(), "import-owner", "browser")
        .await
        .unwrap()
        .unwrap();
    let (status, _) = other.page("/import-owner/browser/migration").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    source.blocked.store(false, Ordering::SeqCst);
    let import_path = "/api/v3/repos/import-owner/browser/import";
    wait_status(&router, import_path, reader, "error").await;
    let html = session.page_ok("/import-owner/browser").await;
    assert!(html.contains("Migration failed"));
    let reply = session
        .form_reply(
            "/import-owner/browser/migration",
            &[
                ("repository_id", &browser.id.to_string()),
                ("source_url", &source.url),
                ("source_username", "upstream"),
                ("source_token", "secret"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    wait_status(&router, import_path, reader, "complete").await;
    let html = session.page_ok("/import-owner/browser").await;
    assert!(!html.contains("Migration in progress"));
    assert!(!html.contains("Migration failed"));
    assert!(html.contains("README.md"));
    let reply = session
        .form_reply(
            "/import-owner/browser/migration",
            &[("repository_id", "0"), ("intent", "cancel")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn github_source_imports() {
    crate::support::fake_webui_bundle::install();
    use codeyard::backend::repositories::imports::source_url;
    assert!(source_url("http://127.0.0.1/repo.git").is_err());
    std::env::set_var("MIGRATIONS_ALLOW_PRIVATE", "true");
    let source = source().await;
    contract(false, &source).await;
    contract(true, &source).await;
}
