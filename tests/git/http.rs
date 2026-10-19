//! Real Git smart-HTTP clients against disposable servers on both databases.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::fake_webui_bundle as bundle;
use crate::support::git_server;
use crate::support::native_database as native;
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{Duration, Utc};
use codeyard::backend::{
    db::{self, repositories, users::tokens, DbPool},
    git::{MAX_MIRROR_REFS, MAX_MIRROR_REF_BYTES, MAX_REPOSITORY_REFS, MAX_REPOSITORY_REF_BYTES},
    repositories::lifecycle,
};
use git_server::{git, success, Server};
use rand::RngCore;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};

fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
async fn create_token(client: &Client, base: &str, cookie: &str, csrf: &str, scope: &str) -> Value {
    let response = client
        .post(format!("{base}/api/user/tokens"))
        .header("Cookie", cookie)
        .header("x-csrf-token", csrf)
        .json(&json!({"name":format!("test-{scope}"),"scope":scope}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["cache-control"], "no-store, private");
    let created: Value = response.json().await.unwrap();
    assert_eq!(created["value"].as_str().unwrap().len(), 64);
    created
}
async fn refused(client: &Client, uri: &str, token: Option<&str>) {
    let mut request = client.get(uri);
    if let Some(token) = token {
        request = request.basic_auth("ignored", Some(token));
    }
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .starts_with("Basic "));
}

async fn read_only_tokens_hide_private_repositories(
    pool: &DbPool,
    client: &Client,
    base: &str,
    owner_token: &str,
) {
    let outsider = db::users::accounts::create_user(pool, "http_outsider", None)
        .await
        .unwrap();
    let session = native::token();
    db::users::sessions::create_session(
        pool,
        &session,
        outsider.id,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let value = native::token();
    tokens::create_token(
        pool,
        outsider.id,
        &session,
        "outsider",
        &hash(&value),
        "read",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    for (method, suffix) in [
        (reqwest::Method::GET, "info/refs?service=git-receive-pack"),
        (reqwest::Method::POST, "git-receive-pack"),
    ] {
        // Receive-pack POSTs are rejected for a missing content type before the
        // repository is looked up, so every request names the type.
        let send = |repo: &str, token: &str| {
            client
                .request(
                    method.clone(),
                    format!("{base}/http_owner/{repo}.git/{suffix}"),
                )
                .header("Content-Type", "application/x-git-receive-pack-request")
                .basic_auth("ignored", Some(token))
                .send()
        };
        let hidden = send("closed", value.as_str()).await.unwrap();
        let missing = send("missing", value.as_str()).await.unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        assert_eq!(hidden.status(), missing.status());
        for header in ["content-type", "www-authenticate"] {
            assert_eq!(hidden.headers().get(header), missing.headers().get(header));
        }
        assert_eq!(
            hidden.bytes().await.unwrap(),
            missing.bytes().await.unwrap()
        );

        for (repo, token) in [("closed", owner_token), ("open", value.as_str())] {
            let response = send(repo, token).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response.text().await.unwrap(), "Token is read-only");
        }
    }
}

async fn primary_packs_after_http_push(root: &Path, disk_path: &str, url: &str, token: &str) {
    let bare = git2::Repository::open_bare(disk_path).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let tree = bare
        .find_tree(bare.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let mut parent = None;
    // Model 50 retained incoming packs, all reachable and with no loose objects.
    for index in 0..50 {
        let parents: Vec<_> = parent.iter().collect();
        let id = bare
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &format!("pack {index}"),
                &tree,
                &parents,
            )
            .unwrap();
        let mut builder = bare.packbuilder().unwrap();
        builder.insert_commit(id).unwrap();
        let mut buffer = git2::Buf::new();
        builder.write_buf(&mut buffer).unwrap();
        let odb = bare.odb().unwrap();
        let mut writer = odb.packwriter().unwrap();
        writer.write_all(&buffer).unwrap();
        writer.commit().unwrap();
        parent = Some(bare.find_commit(id).unwrap());
    }
    success(git(root, &["--git-dir", disk_path, "prune-packed"], None));
    let packs = Path::new(disk_path).join("objects/pack");
    let pack_count = || {
        std::fs::read_dir(&packs)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .count()
    };
    assert_eq!(pack_count(), 50);
    let counts = git(root, &["--git-dir", disk_path, "count-objects", "-v"], None);
    assert!(String::from_utf8(counts.stdout)
        .unwrap()
        .starts_with("count: 0\n"));
    bare.config()
        .unwrap()
        .set_i64("receive.unpackLimit", 1)
        .unwrap();
    success(git(root, &["clone", "-q", url, "packed-client"], None));
    success(git(
        root,
        &[
            "-C",
            "packed-client",
            "commit",
            "--allow-empty",
            "-qm",
            "Pack 51",
        ],
        None,
    ));
    success(git(
        root,
        &["-C", "packed-client", "push", "-q", "origin", "main"],
        Some(token),
    ));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while pack_count() > 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "primary packs were not consolidated after an HTTP push"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    success(git(root, &["clone", "-q", url, "packed-after-gc"], None));
    let history = git(
        root,
        &["-C", "packed-after-gc", "rev-list", "--count", "HEAD"],
        None,
    );
    assert_eq!(String::from_utf8(history.stdout).unwrap().trim(), "51");
}

async fn near_budget_http_clone(root: &Path, disk_path: &str, url: &str) {
    let bare = git2::Repository::open_bare(disk_path).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let tree = bare
        .find_tree(bare.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let commit = bare
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Seed",
            &tree,
            &[],
        )
        .unwrap();
    let target = bare.find_object(commit, None).unwrap();
    let tag = bare
        .tag_annotation_create("annotated", &target, &signature, "Tag")
        .unwrap();
    let mut refs = std::collections::BTreeMap::new();
    refs.insert("refs/heads/main".to_owned(), commit);
    let mut pull_bytes = 0;
    for number in 1..=MAX_MIRROR_REFS - MAX_REPOSITORY_REFS {
        let name = format!("refs/pull/{number}/head");
        pull_bytes += name.len() + 40;
        refs.insert(name, commit);
    }
    let mut public_bytes = "refs/heads/main".len() + 40;
    let prefix = format!("{}/", "t".repeat(99)).repeat(9);
    for number in 0..1000 {
        let name = format!("refs/tags/{prefix}{number:090}");
        assert_eq!(name.len(), 1000);
        public_bytes += name.len() + 40;
        refs.insert(name, tag);
    }
    assert!(public_bytes <= MAX_REPOSITORY_REF_BYTES);
    assert!(pull_bytes <= MAX_MIRROR_REF_BYTES - MAX_REPOSITORY_REF_BYTES);
    assert!(refs.len() - (MAX_MIRROR_REFS - MAX_REPOSITORY_REFS) <= MAX_REPOSITORY_REFS);
    assert!(refs.len() <= MAX_MIRROR_REFS);
    // Write the large fixture as packed refs instead of creating 41,000 loose files.
    let mut packed = std::io::BufWriter::new(
        std::fs::File::create(Path::new(disk_path).join("packed-refs")).unwrap(),
    );
    writeln!(packed, "# pack-refs with: peeled fully-peeled sorted").unwrap();
    for (name, id) in &refs {
        writeln!(packed, "{id} {name}").unwrap();
        if *id == tag {
            writeln!(packed, "^{commit}").unwrap();
        }
    }
    packed.flush().unwrap();
    let response = Client::new()
        .get(format!("{url}/info/refs?service=git-upload-pack"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let advertisement = response.bytes().await.unwrap();
    assert!(advertisement.len() > 4 * 1024 * 1024);
    assert!(advertisement.windows(3).any(|bytes| bytes == b"^{}"));
    success(git(root, &["clone", "-q", url, "many-refs-client"], None));
    let clone = git2::Repository::open(root.join("many-refs-client")).unwrap();
    assert_eq!(clone.head().unwrap().target(), Some(commit));
    assert_eq!(clone.references_glob("refs/tags/*").unwrap().count(), 1000);
    success(git(
        root,
        &[
            "-C",
            "many-refs-client",
            "fetch",
            "-q",
            "origin",
            "refs/pull/40000/head",
        ],
        None,
    ));
    let fetched = git(
        root,
        &["-C", "many-refs-client", "rev-parse", "FETCH_HEAD"],
        None,
    );
    assert_eq!(
        String::from_utf8(fetched.stdout).unwrap().trim(),
        commit.to_string()
    );
}

async fn update(pool: &DbPool, sql: &str, id: i64) {
    match pool {
        DbPool::Postgres(raw) => {
            sqlx::query(sql).bind(id).execute(raw).await.unwrap();
        }
        DbPool::Sqlite(raw) => {
            sqlx::query(sql).bind(id).execute(raw).await.unwrap();
        }
    };
}

async fn buffered_fetch_revalidates(pool: &DbPool, server: &Server, root: &Path) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let owner = db::users::accounts::create_user(pool, "fetch_owner", None)
        .await
        .unwrap();
    for case in [
        "deleted_token",
        "expired_token",
        "banned",
        "password_reset",
        "access_revoked",
        "node_private",
        "repo_private",
        "setup_incomplete",
        "replacement",
        "renamed",
        "unchanged",
    ] {
        let anonymous = matches!(case, "node_private" | "repo_private" | "setup_incomplete");
        let repo = lifecycle::provision(
            (pool, None),
            &root.join("repos"),
            repositories::RepositoryCreationOwner::personal(owner.id),
            case,
            None,
            !anonymous,
            None,
        )
        .await
        .unwrap();
        let commit = {
            let bare = git2::Repository::open_bare(&repo.disk_path).unwrap();
            let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
            let tree = bare
                .find_tree(bare.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            bare.commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Seed",
                &tree,
                &[],
            )
            .unwrap()
        };
        let reader = db::users::accounts::create_user(pool, &format!("fetch_{case}"), None)
            .await
            .unwrap();
        db::repositories::access::grant_repo_access(
            pool,
            reader.id,
            repo.id,
            db::models::AccessLevel::Read,
        )
        .await
        .unwrap();
        let session = native::token();
        db::users::sessions::create_session(
            pool,
            &session,
            reader.id,
            None,
            None,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
        let value = native::token();
        let token =
            tokens::create_token(pool, reader.id, &session, case, &hash(&value), "read", None)
                .await
                .unwrap()
                .unwrap();
        let authorization = if anonymous {
            String::new()
        } else {
            format!(
                "Authorization: Basic {}\r\n",
                STANDARD.encode(format!("ignored:{value}"))
            )
        };
        let refs = Client::new()
            .get(format!(
                "{}/fetch_owner/{case}.git/info/refs?service=git-upload-pack",
                server.base
            ))
            .basic_auth("ignored", Some(&value))
            .send()
            .await
            .unwrap();
        assert_eq!(refs.status(), StatusCode::OK);
        assert!(refs.text().await.unwrap().contains(&commit.to_string()));
        let want = format!("want {commit}\n");
        let body = format!("{:04x}{want}00000009done\n", want.len() + 4).into_bytes();
        let mut stream =
            tokio::net::TcpStream::connect(server.base.strip_prefix("http://").unwrap())
                .await
                .unwrap();
        let headers = format!("POST /fetch_owner/{case}.git/git-upload-pack HTTP/1.1\r\nHost: localhost\r\n{authorization}Content-Type: application/x-git-upload-pack-request\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n", body.len());
        stream.write_all(headers.as_bytes()).await.unwrap();
        // Hyper sends Continue only once the authorized handler polls the body.
        let mut interim = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !interim.ends_with(b"\r\n\r\n") {
                interim.push(stream.read_u8().await.unwrap());
            }
        })
        .await
        .unwrap();
        assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n", "{case}");

        let expected = match case {
            "deleted_token" => {
                assert!(tokens::delete_token(pool, reader.id, token.id)
                    .await
                    .unwrap());
                401
            }
            "expired_token" => {
                update(pool, "UPDATE personal_access_tokens SET expires_at='2000-01-01T00:00:00Z' WHERE id=$1", token.id).await;
                401
            }
            "banned" => {
                update(
                    pool,
                    "UPDATE users SET is_banned=TRUE WHERE id=$1",
                    reader.id,
                )
                .await;
                401
            }
            "password_reset" => {
                update(
                    pool,
                    "UPDATE users SET must_change_password=TRUE WHERE id=$1",
                    reader.id,
                )
                .await;
                401
            }
            "access_revoked" | "replacement" => {
                db::repositories::access::revoke_repo_access(pool, reader.id, repo.id)
                    .await
                    .unwrap();
                if case == "replacement" {
                    update(
                        pool,
                        "UPDATE repositories SET name='old_replacement' WHERE id=$1",
                        repo.id,
                    )
                    .await;
                    let replacement = lifecycle::provision(
                        (pool, None),
                        &root.join("repos"),
                        repositories::RepositoryCreationOwner::personal(owner.id),
                        case,
                        None,
                        false,
                        None,
                    )
                    .await
                    .unwrap();
                    assert_ne!(replacement.id, repo.id);
                }
                404
            }
            "node_private" => {
                update(
                    pool,
                    "UPDATE node_configuration SET node_mode='private' WHERE id=$1",
                    1,
                )
                .await;
                401
            }
            "repo_private" => {
                update(
                    pool,
                    "UPDATE repositories SET is_private=TRUE WHERE id=$1",
                    repo.id,
                )
                .await;
                401
            }
            "setup_incomplete" => {
                update(
                    pool,
                    "UPDATE node_configuration SET setup_completed=FALSE WHERE id=$1",
                    1,
                )
                .await;
                404
            }
            "renamed" => {
                update(
                    pool,
                    "UPDATE repositories SET name='after_rename' WHERE id=$1",
                    repo.id,
                )
                .await;
                200
            }
            "unchanged" => 200,
            _ => unreachable!(),
        };
        stream.write_all(&body).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            stream.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            response.starts_with(format!("HTTP/1.1 {expected} ").as_bytes()),
            "{case}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(
            response.windows(4).any(|bytes| bytes == b"PACK"),
            expected == 200,
            "{case}"
        );
        update(
            pool,
            "UPDATE node_configuration SET node_mode='public',setup_completed=TRUE WHERE id=$1",
            1,
        )
        .await;
    }
}

async fn archive_tag_budgets(client: &Client, base: &str, disk_path: &str) {
    let repo = git2::Repository::open_bare(disk_path).unwrap();
    let commit = repo.refname_to_id("refs/heads/main").unwrap();
    // Depth and total materialization are separate limits; every tag in the byte
    // budget cases is below the individual 256 KiB object limit.
    for (name, depth, size, expected) in [
        ("depth-limit", 32, 128, StatusCode::OK),
        ("depth-over", 33, 128, StatusCode::PAYLOAD_TOO_LARGE),
        ("bytes-limit", 8, 128 * 1024, StatusCode::OK),
        ("bytes-over", 9, 128 * 1024, StatusCode::PAYLOAD_TOO_LARGE),
        ("object-limit", 1, 256 * 1024, StatusCode::OK),
        (
            "object-over",
            1,
            256 * 1024 + 1,
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let mut target = commit;
        for index in 0..depth {
            let kind = if index == 0 { "commit" } else { "tag" };
            let mut object = format!("object {target}\ntype {kind}\ntag nested\n\n");
            object.push_str(&"x".repeat(size - object.len()));
            target = repo
                .odb()
                .unwrap()
                .write(git2::ObjectType::Tag, object.as_bytes())
                .unwrap();
        }
        let reference = format!("refs/tags/{name}");
        repo.reference(&reference, target, false, "fixture")
            .unwrap();
        for (path, query) in [
            (
                "/api/repos/http_owner/open/archive".to_owned(),
                vec![("ref", name)],
            ),
            (format!("/http_owner/open/archive/{name}.zip"), vec![]),
        ] {
            let response = client
                .get(format!("{base}{path}"))
                .query(&query)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}?ref={name}");
            if expected == StatusCode::OK {
                assert!(response.bytes().await.unwrap().starts_with(b"PK"));
            }
        }
        repo.find_reference(&reference).unwrap().delete().unwrap();
    }
}

async fn exercise(pool: &DbPool, url: &str) {
    bundle::install();
    match pool {
        DbPool::Postgres(raw) => {
            sqlx::query("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE").execute(raw).await.unwrap();
        }
        DbPool::Sqlite(raw) => {
            sqlx::query("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE").execute(raw).await.unwrap();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let owner = db::users::accounts::create_user(pool, "http_owner", None)
        .await
        .unwrap();
    let public = lifecycle::provision(
        (pool, None),
        &root.path().join("repos"),
        repositories::RepositoryCreationOwner::personal(owner.id),
        "open",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let private = lifecycle::provision(
        (pool, None),
        &root.path().join("repos"),
        repositories::RepositoryCreationOwner::personal(owner.id),
        "closed",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let session = native::token();
    db::users::sessions::create_session(
        pool,
        &session,
        owner.id,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let cookie = format!("session_id={session}");
    let csrf = native::token();
    assert_eq!(
        db::users::sessions::get_or_create_csrf_token(pool, &session, Some(owner.id), &csrf, 3600)
            .await
            .unwrap(),
        Some(csrf.clone())
    );
    let server = Server::start(root.path(), url).await;
    buffered_fetch_revalidates(pool, &server, root.path()).await;
    let client = Client::new();
    let public_url = format!("{}/http_owner/open.git", server.base);
    let private_url = format!("{}/http_owner/closed.git", server.base);
    let refs = format!("{private_url}/info/refs?service=git-upload-pack");
    refused(&client, &refs, None).await;
    refused(&client, &refs, Some("not-a-token")).await;
    // A session and a password are not Git credentials.
    assert_eq!(
        client
            .get(&refs)
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let read = create_token(&client, &server.base, &cookie, &csrf, "read").await;
    let write = create_token(&client, &server.base, &cookie, &csrf, "write").await;
    let read_value = read["value"].as_str().unwrap();
    let write_value = write["value"].as_str().unwrap();
    read_only_tokens_hide_private_repositories(pool, &client, &server.base, read_value).await;
    assert!(tokens::find_token(pool, read_value)
        .await
        .unwrap()
        .is_none());
    assert!(tokens::find_token(pool, &hash(read_value))
        .await
        .unwrap()
        .is_some());
    let listing: Value = client
        .get(format!("{}/api/user/tokens", server.base))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing.as_array().unwrap().len(), 2);
    assert!(!listing.to_string().contains(read_value));
    assert!(!listing.to_string().contains("token_hash"));
    assert_eq!(
        client
            .get(format!("{}/api/user/tokens", server.base))
            .basic_auth("ignored", Some(write_value))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // Native settings and hydrated page-model navigation use the same token view.
    for accept in ["text/html", "application/vnd.codeyard.page+json"] {
        let response = client
            .get(format!("{}/settings?tab=tokens", server.base))
            .header("Cookie", &cookie)
            .header("Accept", accept)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("test-read"));
        assert!(!body.contains(read_value));
    }
    success(git(
        root.path(),
        &["clone", "-q", &public_url, "anonymous"],
        None,
    ));
    success(git(
        root.path(),
        &["clone", "-q", &private_url, "reader"],
        Some(read_value),
    ));
    success(git(
        root.path(),
        &["-C", "reader", "checkout", "-b", "main"],
        None,
    ));
    // An incompressible pack above Axum's default body limit, sent chunked.
    let mut blob = vec![0u8; 3 * 1024 * 1024];
    rand::rngs::OsRng.fill_bytes(&mut blob);
    std::fs::write(root.path().join("reader/blob"), blob).unwrap();
    success(git(root.path(), &["-C", "reader", "add", "blob"], None));
    success(git(
        root.path(),
        &["-C", "reader", "commit", "-qm", "HTTP push"],
        None,
    ));
    assert!(!git(
        root.path(),
        &["-C", "reader", "push", "-q", "origin", "main"],
        Some(read_value)
    )
    .status
    .success());
    assert!(git2::Repository::open_bare(&private.disk_path)
        .unwrap()
        .find_reference("refs/heads/main")
        .is_err());
    success(git(
        root.path(),
        &["-C", "reader", "push", "-q", "origin", "main"],
        Some(write_value),
    ));
    let local =
        String::from_utf8(git(root.path(), &["-C", "reader", "rev-parse", "HEAD"], None).stdout)
            .unwrap();
    assert_eq!(
        git2::Repository::open_bare(&private.disk_path)
            .unwrap()
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        local.trim()
    );
    success(git(
        root.path(),
        &["clone", "-q", &private_url, "reader-after-push"],
        Some(read_value),
    ));
    // Also populate the public repo so anonymous clone exercises upload-pack.
    success(git(
        root.path(),
        &["-C", "reader", "push", "-q", &public_url, "main"],
        Some(write_value),
    ));
    success(git(
        root.path(),
        &["clone", "-q", &public_url, "anonymous-after-push"],
        None,
    ));
    assert!(root.path().join("anonymous-after-push/blob").is_file());

    // First-push HEAD, stored metadata and clone checkout agree, even beyond the old
    // 255-character database limit. Each path component still fits the filesystem.
    for (name, branch) in [
        ("trunk-only", "trunk".to_owned()),
        (
            "long-branch",
            format!("{}/{}", "a".repeat(128), "b".repeat(128)),
        ),
    ] {
        let repository = lifecycle::provision(
            (pool, None),
            &root.path().join("repos"),
            repositories::RepositoryCreationOwner::personal(owner.id),
            name,
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let repo_url = format!("{}/http_owner/{name}.git", server.base);
        let source = format!("{name}-client");
        let clone = format!("{name}-clone");
        success(git(
            root.path(),
            &["init", "-q", "-b", &branch, &source],
            None,
        ));
        std::fs::write(root.path().join(&source).join("README"), "first push\n").unwrap();
        success(git(root.path(), &["-C", &source, "add", "."], None));
        success(git(
            root.path(),
            &["-C", &source, "commit", "-qm", "First"],
            None,
        ));
        success(git(
            root.path(),
            &["-C", &source, "push", "-q", &repo_url, &branch],
            Some(write_value),
        ));
        let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
        assert_eq!(
            bare.find_reference("HEAD").unwrap().symbolic_target(),
            Some(format!("refs/heads/{branch}").as_str())
        );
        success(git(
            root.path(),
            &["clone", "-q", &repo_url, &clone],
            Some(read_value),
        ));
        assert!(root.path().join(&clone).join("README").is_file());
        let head = git(
            root.path(),
            &["-C", &clone, "symbolic-ref", "--short", "HEAD"],
            None,
        );
        assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), branch);
        let stored = repositories::records::get_repository_by_id(pool, repository.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.default_branch, branch);
        assert!(stored.size_bytes > 0);
        let repo: Value = client
            .get(format!("{}/api/repos/http_owner/{name}", server.base))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(repo["default_branch"], branch);
        success(git(
            root.path(),
            &["-C", &source, "commit", "--allow-empty", "-qm", "Second"],
            None,
        ));
        success(git(
            root.path(),
            &["-C", &source, "push", "-q", &repo_url, &branch],
            Some(write_value),
        ));
        let updated = repositories::records::get_repository_by_id(pool, repository.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.default_branch, branch);
        assert!(updated.size_bytes > stored.size_bytes);
        assert!(!git(
            root.path(),
            &["-C", &source, "push", &repo_url, &format!(":{branch}")],
            Some(write_value)
        )
        .status
        .success());
    }

    let packed = lifecycle::provision(
        (pool, None),
        &root.path().join("repos"),
        repositories::RepositoryCreationOwner::personal(owner.id),
        "packed",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    primary_packs_after_http_push(
        root.path(),
        &packed.disk_path,
        &format!("{}/http_owner/packed.git", server.base),
        write_value,
    )
    .await;

    let many_refs = lifecycle::provision(
        (pool, None),
        &root.path().join("repos"),
        repositories::RepositoryCreationOwner::personal(owner.id),
        "many-refs",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    near_budget_http_clone(
        root.path(),
        &many_refs.disk_path,
        &format!("{}/http_owner/many-refs.git", server.base),
    )
    .await;

    // Anonymous request bodies can hold every Git session slot. Ref advertisement
    // must use the same permit instead of spawning an unbounded extra process.
    let address = server
        .base
        .strip_prefix("http://")
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    // Hyper sends `100 Continue` only after the handler polls the body, which happens
    // once the request holds its Git session permit. A refused request gets its 503
    // instead. Waiting for each answer fills all sixteen permits before the probe runs,
    // so the probe cannot take one of them.
    let mut stalled = Vec::new();
    let mut last_status_line = String::new();
    for _ in 0..100 {
        if stalled.len() == 16 {
            break;
        }
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"POST /http_owner/open.git/git-upload-pack HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-git-upload-pack-request\r\nContent-Length: 1\r\nExpect: 100-continue\r\n\r\n",
        )
        .await
        .unwrap();
        let mut status = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !status.ends_with(b"\r\n") {
                status.push(tokio::io::AsyncReadExt::read_u8(&mut stream).await.unwrap());
            }
        })
        .await
        .unwrap();
        if status == b"HTTP/1.1 100 Continue\r\n" {
            stalled.push(stream);
        } else {
            // A permit held by the previous request may still be releasing.
            last_status_line = String::from_utf8_lossy(&status).into_owned();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    assert_eq!(
        stalled.len(),
        16,
        "last refused upload: {last_status_line:?}"
    );
    let public_refs = format!("{public_url}/info/refs?service=git-upload-pack");
    let response = client.get(&public_refs).send().await.unwrap();
    let busy_status = response.status();
    let busy_body = response.text().await.unwrap();
    assert_eq!(
        (busy_status, busy_body.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "Git sessions busy")
    );
    drop(stalled);
    let mut last = String::from("no response");
    let mut recovered = false;
    for _ in 0..100 {
        match client.get(&public_refs).send().await {
            Ok(response) if response.status() == StatusCode::OK => {
                recovered = true;
                break;
            }
            Ok(response) => {
                last = format!(
                    "{}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                );
            }
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        recovered,
        "Git session permits did not recover; last response {last}"
    );

    assert!(
        db::repositories::records::get_repository_by_id(pool, public.id)
            .await
            .unwrap()
            .unwrap()
            .size_bytes
            > 0
    );
    // A repository whose name ends in .git must not resolve to the shorter name.
    let suffixed = lifecycle::provision(
        (pool, None),
        &root.path().join("repos"),
        repositories::RepositoryCreationOwner::personal(owner.id),
        "open.git",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let suffixed_url = format!("{}/http_owner/open.git.git", server.base);
    success(git(
        root.path(),
        &[
            "-C",
            "reader",
            "commit",
            "--allow-empty",
            "-qm",
            "suffixed repository",
        ],
        None,
    ));
    let suffix_head =
        String::from_utf8(git(root.path(), &["-C", "reader", "rev-parse", "HEAD"], None).stdout)
            .unwrap();
    success(git(
        root.path(),
        &["-C", "reader", "push", "-q", &suffixed_url, "HEAD:main"],
        Some(write_value),
    ));
    for (path, expected) in [
        (&public.disk_path, local.trim()),
        (&suffixed.disk_path, suffix_head.trim()),
    ] {
        assert_eq!(
            git2::Repository::open_bare(path)
                .unwrap()
                .find_reference("refs/heads/main")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            expected
        );
    }
    success(git(
        root.path(),
        &["clone", "-q", &suffixed_url, "suffixed-clone"],
        None,
    ));
    assert_eq!(
        String::from_utf8(
            git(
                root.path(),
                &["-C", "suffixed-clone", "rev-parse", "HEAD"],
                None
            )
            .stdout
        )
        .unwrap(),
        suffix_head
    );

    let raw_url = format!("{}/api/repos/http_owner/open/raw/main/blob", server.base);
    let response = client
        .get(&raw_url)
        .header("Range", "bytes=10-25")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["accept-ranges"], "bytes");
    assert_eq!(response.headers()["content-range"], "bytes 10-25/3145728");
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        &std::fs::read(root.path().join("reader/blob")).unwrap()[10..26]
    );

    // The anonymous archive endpoint rejects options without changing bare-repo files.
    let archive_url = format!("{}/api/repos/http_owner/open/archive", server.base);
    let head_before = std::fs::read(Path::new(&public.disk_path).join("HEAD")).unwrap();
    let response = client
        .get(&archive_url)
        .query(&[("ref", "--output=HEAD")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        std::fs::read(Path::new(&public.disk_path).join("HEAD")).unwrap(),
        head_before
    );
    let response = client
        .get(&archive_url)
        .query(&[("ref", "main")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.bytes().await.unwrap().starts_with(b"PK"));

    archive_tag_budgets(&client, &server.base, &public.disk_path).await;

    // Valid option-looking refs pushed by an owner remain safe for anonymous reads.
    assert_eq!(
        git2::Repository::open_bare(&public.disk_path)
            .unwrap()
            .find_commit(git2::Oid::from_str(local.trim()).unwrap())
            .unwrap()
            .parent_count(),
        0
    );
    for (namespace, revision) in [
        ("heads", "--output=read-branch"),
        ("tags", "--output=read-tag"),
    ] {
        let sentinel =
            Path::new(&public.disk_path).join(revision.strip_prefix("--output=").unwrap());
        std::fs::write(&sentinel, b"keep this file\n").unwrap();
        success(git(
            root.path(),
            &[
                "-C",
                "reader",
                "push",
                "-q",
                &public_url,
                &format!("{}:refs/{namespace}/{revision}", local.trim()),
            ],
            Some(write_value),
        ));
        let response = client
            .get(format!(
                "{}/api/repos/http_owner/open/tree/{revision}",
                server.base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let tree: Value = response.json().await.unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep this file\n");
        assert_eq!(
            std::fs::read(Path::new(&public.disk_path).join("HEAD")).unwrap(),
            head_before
        );
        assert_eq!(tree.as_array().unwrap().len(), 1);
        assert_eq!(tree[0]["path"], "blob");
        assert_eq!(tree[0]["last_commit"]["sha"], local.trim());

        let response = client
            .get(format!(
                "{}/api/repos/http_owner/open/commits/{revision}",
                server.base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let commit: Value = response.json().await.unwrap();
        assert_eq!(commit["commit"]["sha"], local.trim());
        assert_eq!(commit["diff"]["files"].as_array().unwrap().len(), 1);
        assert_eq!(commit["diff"]["files"][0]["filename"], "blob");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep this file\n");
        assert_eq!(
            std::fs::read(Path::new(&public.disk_path).join("HEAD")).unwrap(),
            head_before
        );
    }

    // API branch operations must obey the same protection as Git pushes.
    let branches_url = format!("{}/api/repos/http_owner/open/branches", server.base);
    let response = client
        .post(&branches_url)
        .header("Cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&json!({"name":"release/v1", "sha":local.trim()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    db::repositories::protection::upsert_branch_protection_rule(
        pool,
        repositories::BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: public.id,
            pattern: "release/*",
            require_pull_request: true,
            required_approving_reviews: 0,
            dismiss_stale_reviews: false,
            require_status_checks: false,
            required_status_checks: vec![],
            require_up_to_date_branch: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let response = client
        .delete(format!("{branches_url}/release%2Fv1"))
        .header("Cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = client
        .post(&branches_url)
        .header("Cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&json!({"name":"release/v2", "sha":local.trim()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bare = git2::Repository::open_bare(&public.disk_path).unwrap();
    assert_eq!(
        bare.find_reference("refs/heads/release/v1")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        local.trim()
    );
    assert!(bare.find_reference("refs/heads/release/v2").is_err());
    let response = client
        .post(&branches_url)
        .header("Cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&json!({"name":"feature", "sha":local.trim()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = client
        .delete(format!("{branches_url}/feature"))
        .header("Cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(bare.find_reference("refs/heads/feature").is_err());
    drop(bare);

    // Gzip-compressed upload RPCs remain valid, with the decompressed size bounded.
    let want = format!("want {}\n", local.trim());
    let request = format!("{:04x}{want}00000009done\n", want.len() + 4);
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(request.as_bytes()).unwrap();
    let response = client
        .post(format!("{private_url}/git-upload-pack"))
        .basic_auth("ignored", Some(read_value))
        .header("Content-Type", "application/x-git-upload-pack-request")
        .header("Content-Encoding", "gzip")
        .body(gzip.finish().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .bytes()
        .await
        .unwrap()
        .windows(4)
        .any(|bytes| bytes == b"PACK"));
    for encoding in ["gzip", "br", "gzip, identity"] {
        let response = client
            .post(format!("{private_url}/git-receive-pack"))
            .basic_auth("ignored", Some(write_value))
            .header("Content-Type", "application/x-git-receive-pack-request")
            .header("Content-Encoding", encoding)
            .body("0000")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let oversized = vec![b'x'; 16 * 1024 * 1024 + 1];
    for encoding in ["identity", "gzip"] {
        let body = if encoding == "gzip" {
            let mut gzip =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gzip.write_all(&oversized).unwrap();
            gzip.finish().unwrap()
        } else {
            oversized.clone()
        };
        let response = client
            .post(format!("{private_url}/git-upload-pack"))
            .basic_auth("ignored", Some(read_value))
            .header("Content-Type", "application/x-git-upload-pack-request")
            .header("Content-Encoding", encoding)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
    db::repositories::protection::upsert_branch_protection_rule(
        pool,
        repositories::BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: private.id,
            pattern: "main",
            require_pull_request: true,
            required_approving_reviews: 0,
            dismiss_stale_reviews: false,
            require_status_checks: false,
            required_status_checks: vec![],
            require_up_to_date_branch: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    success(git(
        root.path(),
        &[
            "-C",
            "reader",
            "commit",
            "--allow-empty",
            "-qm",
            "protected",
        ],
        None,
    ));
    let rejected = git(
        root.path(),
        &["-C", "reader", "push", "origin", "main"],
        Some(write_value),
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("pre-receive hook declined"));
    assert_eq!(
        git2::Repository::open_bare(&private.disk_path)
            .unwrap()
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        local.trim()
    );
    let expired = native::token();
    tokens::create_token(
        pool,
        owner.id,
        &session,
        "expired",
        &hash(&expired),
        "read",
        Some(Utc::now() - Duration::seconds(1)),
    )
    .await
    .unwrap()
    .unwrap();
    refused(&client, &refs, Some(&expired)).await;
    let deleted = client
        .delete(format!("{}/api/user/tokens/{}", server.base, read["id"]))
        .header("Cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    refused(&client, &refs, Some(read_value)).await;
    db::users::accounts::update_user_password(pool, owner.id, "unused-hash", true)
        .await
        .unwrap();
    refused(&client, &refs, Some(write_value)).await;
    assert!(tokens::create_token(
        pool,
        owner.id,
        &session,
        "reset",
        &hash(&native::token()),
        "write",
        None
    )
    .await
    .unwrap()
    .is_none());
    let banned = db::users::accounts::create_user(pool, "banned", None)
        .await
        .unwrap();
    let banned_session = native::token();
    db::users::sessions::create_session(
        pool,
        &banned_session,
        banned.id,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let banned_value = native::token();
    tokens::create_token(
        pool,
        banned.id,
        &banned_session,
        "banned",
        &hash(&banned_value),
        "read",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    match pool {
        DbPool::Postgres(raw) => {
            sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=$1")
                .bind(banned.id)
                .execute(raw)
                .await
                .unwrap();
        }
        DbPool::Sqlite(raw) => {
            sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=?1")
                .bind(banned.id)
                .execute(raw)
                .await
                .unwrap();
        }
    }
    refused(&client, &refs, Some(&banned_value)).await;
    // Unknown-token failures share the existing account/global password limits.
    let mut limited = false;
    for _ in 0..12 {
        let response = client
            .get(&refs)
            .basic_auth("ignored", Some("bad-password"))
            .send()
            .await
            .unwrap();
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(limited);
    let logs = std::fs::read_to_string(&server.log).unwrap();
    for value in [read_value, write_value, &expired, &banned_value] {
        assert!(!logs.contains(value), "token leaked into server logs");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_http_sqlite() {
    let database = native::NativeDatabase::new().await;
    exercise(&database.pool, &database.url).await;
    database.close().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_http_postgres() {
    let database = native::PostgresDatabase::new().await;
    let DbPool::Postgres(raw) = &database.pool else {
        unreachable!()
    };
    let schema: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(raw)
        .await
        .unwrap();
    let mut url = url::Url::parse(&std::env::var("GITSERV_TEST_DATABASE_URL").unwrap()).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-c search_path={schema}"));
    exercise(&database.pool, url.as_str()).await;
}
