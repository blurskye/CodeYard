//! Real gist smart HTTP, built-in SSH, remote publication, and promoted-copy recovery.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::{
    fake_webui_bundle as bundle,
    git_remote::{git, success, tool, Server},
    git_server::write_token,
    github_spec::assert_github_schema,
    native_database as native,
};
use codeyard::backend::cluster::replicas;
use codeyard::backend::db;
use codeyard::backend::db::users::accounts::AccountCredential;
use codeyard_model::repositories::DeployKeyError;
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

// libgit2's signature writer truncates to u32. Write actual wide dates into the object,
// then verify the parser sees them, rather than accidentally testing a wrapped date.
fn dated_commit(work: &Path, timestamp: i64, message: &str) {
    let repo = git2::Repository::open(work).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let tree = repo.index().unwrap().write_tree().unwrap();
    let bytes=format!("tree {tree}\nparent {}\nauthor Alice <alice@users.noreply> {timestamp} +0000\ncommitter Alice <alice@users.noreply> {timestamp} +0000\n\n{message}\n",parent.id());
    let id = repo
        .odb()
        .unwrap()
        .write(git2::ObjectType::Commit, bytes.as_bytes())
        .unwrap();
    assert_eq!(repo.find_commit(id).unwrap().time().seconds(), timestamp);
    repo.reference("refs/heads/main", id, true, "Dated fixture commit")
        .unwrap();
}
async fn keypair(root: &Path, name: &str) -> (PathBuf, String) {
    let path = root.join(name);
    let output = Command::new(tool("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&path)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let public = std::fs::read_to_string(path.with_extension("pub")).unwrap();
    (path, public)
}
async fn api(
    web: &Server,
    token: Option<&str>,
    operation: &str,
    method: Method,
    path: &str,
    body: Value,
    status: StatusCode,
) -> Value {
    let client = Client::new();
    let mut request = client
        .request(method, format!("{}{path}", web.base))
        .json(&body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let reply = request.send().await.unwrap();
    let actual = reply.status();
    let bytes = reply.bytes().await.unwrap();
    assert_eq!(
        actual,
        status,
        "{path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        return Value::Null;
    }
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_github_schema(operation, status.as_u16(), &value);
    value
}
fn detached(path: &Path, base: &str) -> String {
    let repo = git2::Repository::open_bare(path).unwrap();
    let parent = repo.find_commit(base.parse().unwrap()).unwrap();
    let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    builder
        .insert(
            "recovered.txt",
            repo.blob(b"published before promotion\n").unwrap(),
            0o100644,
        )
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let author = git2::Signature::now("Fixture", "remote@example.invalid").unwrap();
    repo.commit(
        None,
        &author,
        &author,
        "Interrupted publication",
        &tree,
        &[&parent],
    )
    .unwrap()
    .to_string()
}
async fn current_copies(database: &native::PostgresDatabase, repositories: &[i64], target: i64) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut current = true;
            for id in repositories {
                let version: i64 =
                    sqlx::query_scalar("SELECT write_version FROM repositories WHERE id=$1")
                        .bind(id)
                        .fetch_one(&database.raw)
                        .await
                        .unwrap();
                current &= db::cluster::list_replicas(&database.pool, *id)
                    .await
                    .unwrap()
                    .iter()
                    .any(|(node, applied)| *node == target && *applied >= version);
            }
            if current {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("gist replicas did not catch up");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gists_http_ssh_remote_and_promotion() {
    bundle::install();
    let mut database = native::PostgresDatabase::new().await;
    codeyard::backend::db::cluster::enable_cluster(&database.pool)
        .await
        .unwrap();
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let alice = db::users::accounts::create_user(&database.pool, "gist_alice", None)
        .await
        .unwrap();
    let bob = db::users::accounts::create_user(&database.pool, "gist_bob", None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET is_admin=TRUE WHERE id=$1")
        .bind(bob.id)
        .execute(&database.raw)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (key, public) = keypair(root.path(), "alice-key").await;
    let account_key = db::users::ssh_keys::add_ssh_key(&database.pool, alice.id, "gists", &public)
        .await
        .unwrap();
    let (deploy, deploy_public) = keypair(root.path(), "deploy-key").await;
    let token = write_token(&database.pool, alice.id).await;
    let bob_token = write_token(&database.pool, bob.id).await;
    let storage_root = root.path().join("storage");
    let storage = Server::start(
        &storage_root,
        &database,
        "gist-storage",
        "web,storage",
        alice.id,
    )
    .await;
    let web = Server::start(
        &root.path().join("web"),
        &database,
        "gist-web",
        "web,ssh",
        alice.id,
    )
    .await;
    let created = api(
        &web,
        Some(&token),
        "gists/create",
        Method::POST,
        "/api/v3/gists",
        json!({"public":false,"description":"Transport","files":{"a.txt":{"content":"start\n"}}}),
        StatusCode::CREATED,
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let path = format!("/api/v3/gists/{id}");
    let metadata = db::gists::get(&database.pool, id).await.unwrap().unwrap();
    let repo = db::repositories::records::get_repository_by_id(&database.pool, metadata.repo_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repo.storage_node_id, Some(storage.node_id));
    let uuid = codeyard::backend::cluster::rpc::repository_uuid(&repo.disk_path).unwrap();
    let disk = storage_root.join("repos").join(format!("{uuid}.git"));
    assert!(disk.is_dir());
    let http = format!("{}/gists/{id}.git", web.base);
    let ssh = format!("ssh://git@127.0.0.1:{}/gists/{id}.git", web.ssh_port);
    success(git(root.path(), &["clone", &http, "client"], None, &key).await);
    let work = root.path().join("client");
    assert_eq!(
        std::fs::read_to_string(work.join("a.txt")).unwrap(),
        "start\n"
    );
    std::fs::write(work.join("a.txt"), "pushed through HTTP\n").unwrap();
    std::fs::write(work.join("binary.dat"), b"\0\x01\xff").unwrap();
    std::fs::set_permissions(work.join("a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
    success(git(&work, &["add", "-A"], None, &key).await);
    success(git(&work, &["update-index", "--chmod=+x", "a.txt"], None, &key).await);
    dated_commit(&work, 253402300799, "HTTP publication in year 9999");
    success(git(&work, &["push", "origin", "main"], Some(&token), &key).await);
    let pushed = api(
        &web,
        None,
        "gists/get",
        Method::GET,
        &path,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(pushed["files"]["a.txt"]["content"], "pushed through HTTP\n");
    assert_eq!(
        pushed["history"][0]["committed_at"],
        "9999-12-31T23:59:59+00:00"
    );
    assert_eq!(pushed["files"]["binary.dat"]["encoding"], "base64");
    assert_eq!(pushed["files"]["binary.dat"]["content"], "AAH/");
    assert_ne!(pushed["updated_at"], created["updated_at"]);
    std::fs::write(work.join("a.txt"), "SSH publication\n").unwrap();
    success(git(&work, &["add", "a.txt"], None, &key).await);
    success(git(&work, &["commit", "-m", "SSH publication"], None, &key).await);
    success(git(&work, &["push", &ssh, "main"], None, &key).await);
    let edited = api(
        &web,
        Some(&token),
        "gists/update",
        Method::PATCH,
        &path,
        json!({"files":{"a.txt":{"content":"Edited through REST\n"}}}),
        StatusCode::OK,
    )
    .await;
    {
        let git = git2::Repository::open_bare(&disk).unwrap();
        let head = git.head().unwrap();
        assert_eq!(
            edited["history"][0]["version"],
            head.target().unwrap().to_string(),
            "a future-dated ancestor must not replace the current revision"
        );
        let tree = head.peel_to_tree().unwrap();
        assert_eq!(tree.get_name("a.txt").unwrap().filemode(), 0o100755);
    }
    assert_eq!(
        edited["history"][2]["committed_at"],
        "9999-12-31T23:59:59+00:00"
    );
    let stable = edited["history"][0]["version"].as_str().unwrap();
    let stable_updated = edited["updated_at"].clone();
    // Even a site administrator cannot acquire another account's gist write permission.
    success(git(&work, &["fetch", "origin"], None, &key).await);
    success(git(&work, &["reset", "--hard", "origin/main"], None, &key).await);
    std::fs::write(work.join("a.txt"), "Not the owner\n").unwrap();
    success(git(&work, &["add", "a.txt"], None, &key).await);
    success(git(&work, &["commit", "-m", "Unauthorized"], None, &key).await);
    assert!(
        !git(&work, &["push", "origin", "main"], Some(&bob_token), &key)
            .await
            .status
            .success()
    );
    for reference in [
        "HEAD:refs/heads/extra",
        "HEAD:refs/tags/nope",
        ":refs/heads/main",
    ] {
        assert!(
            !git(&work, &["push", "origin", reference], Some(&token), &key)
                .await
                .status
                .success()
        );
    }
    // Validate all newly introduced commits, not just a valid final tree after an invalid parent.
    for case in [
        "nested",
        "symlink",
        "empty",
        "size",
        "count",
        "history",
        "past-history",
        "timestamp",
        "commit_size",
    ] {
        success(git(&work, &["reset", "--hard", "origin/main"], None, &key).await);
        success(git(&work, &["clean", "-fd"], None, &key).await);
        match case {
            "nested" | "history" | "past-history" => {
                std::fs::create_dir(work.join("nested")).unwrap();
                std::fs::write(work.join("nested/bad"), "not flat").unwrap();
            }
            "symlink" => std::os::unix::fs::symlink("a.txt", work.join("link")).unwrap(),
            "empty" => {
                success(git(&work, &["rm", "-r", "--", "."], None, &key).await);
            }
            "timestamp" => std::fs::write(work.join("a.txt"), "unrepresentable date").unwrap(),
            "commit_size" => {}
            "size" => std::fs::write(work.join("large"), vec![b'x'; 10 * 1024 * 1024 + 1]).unwrap(),
            "count" => {
                for index in 0..101 {
                    std::fs::write(work.join(format!("{index}.txt")), "x").unwrap();
                }
            }
            _ => unreachable!(),
        }
        success(git(&work, &["add", "-A"], None, &key).await);
        if case == "commit_size" {
            dated_commit(&work, 1, &"x".repeat(64 * 1024 * 1024));
        } else if matches!(case, "timestamp" | "past-history") {
            let timestamp = if case == "timestamp" { 253402300800 } else { 1 };
            dated_commit(&work, timestamp, case);
        } else {
            success(git(&work, &["commit", "-m", case], None, &key).await);
        }
        if matches!(case, "history" | "past-history") {
            std::fs::remove_dir_all(work.join("nested")).unwrap();
            success(git(&work, &["add", "-A"], None, &key).await);
            success(git(&work, &["commit", "-m", "Valid final tree"], None, &key).await);
        } else if case == "commit_size" {
            success(
                git(
                    &work,
                    &["commit", "--allow-empty", "-m", "Valid final tree"],
                    None,
                    &key,
                )
                .await,
            );
        }
        assert!(
            !git(&work, &["push", "origin", "main"], Some(&token), &key)
                .await
                .status
                .success(),
            "{case} was accepted"
        );
        let current = api(
            &web,
            None,
            "gists/get",
            Method::GET,
            &path,
            Value::Null,
            StatusCode::OK,
        )
        .await;
        assert_eq!(current["history"][0]["version"], stable);
        assert_eq!(current["updated_at"], stable_updated);
    }
    // Gists do not expose ordinary deploy-key administration; imported keys cannot gain access.
    let sessions = db::users::sessions::list_user_sessions(&database.pool, alice.id)
        .await
        .unwrap();
    let session = &sessions[0].id;
    let fingerprint = db::users::ssh_keys::calculate_fingerprint(&deploy_public).unwrap();
    let deployment = db::deploy_keys::NewDeployKey {
        repo_id: repo.id,
        title: "legacy".into(),
        fingerprint: fingerprint.clone(),
        public_key: deploy_public.clone(),
        can_write: true,
        created_by_id: alice.id,
    };
    let refusal = db::deploy_keys::insert_deploy_key(&database.pool, &deployment, session)
        .await
        .err()
        .unwrap();
    assert!(matches!(
        refusal.downcast_ref::<DeployKeyError>(),
        Some(DeployKeyError::Denied)
    ));
    sqlx::query("INSERT INTO deploy_keys(repo_id,name,fingerprint,public_key,read_only,created_by_id) VALUES($1,'legacy',$2,$3,FALSE,$4)").bind(repo.id).bind(fingerprint).bind(deploy_public).bind(alice.id).execute(&database.raw).await.unwrap();
    assert!(!git(root.path(), &["ls-remote", &ssh], None, &deploy)
        .await
        .status
        .success());
    assert!(!git(&work, &["push", &ssh, "main"], None, &deploy)
        .await
        .status
        .success());
    let fork = api(
        &web,
        Some(&bob_token),
        "gists/fork",
        Method::POST,
        &format!("{path}/forks"),
        Value::Null,
        StatusCode::CREATED,
    )
    .await;
    let fork_id = fork["id"].as_str().unwrap();
    let fork_meta = db::gists::get(&database.pool, fork_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fork["public"], false);
    let copy = Server::start(
        &root.path().join("copy"),
        &database,
        "gist-copy",
        "storage",
        alice.id,
    )
    .await;
    db::cluster::set_replicas(&database.pool, "2")
        .await
        .unwrap();
    current_copies(&database, &[repo.id, fork_meta.repo_id], copy.node_id).await;
    // Simulate a crash after exact Git publication but before final metadata acknowledgement.
    let sha = detached(&disk, stable);
    db::gists::admit(
        &database.pool,
        alice.id,
        &AccountCredential::Token(
            db::users::tokens::find_token(
                &database.pool,
                &hex::encode(Sha256::digest(token.as_bytes())),
            )
            .await
            .unwrap()
            .unwrap()
            .id,
            &hex::encode(Sha256::digest(token.as_bytes())),
        ),
        &db::gists::Admission {
            repo_id: repo.id,
            disk_path: &repo.disk_path,
            storage_node_id: Some(Some(storage.node_id)),
            base: Some(stable),
            sha: &sha,
            description: Some("Recovered after promotion"),
        },
    )
    .await
    .unwrap();
    git2::Repository::open_bare(&disk)
        .unwrap()
        .reference_matching(
            "refs/heads/main",
            sha.parse().unwrap(),
            true,
            stable.parse().unwrap(),
            "interrupted publication",
        )
        .unwrap();
    // The fixture cannot hold the child process's mutation guard: advance the version only
    // after publishing so its copy loop cannot advertise the old tree at the new version.
    assert!(
        db::cluster::bump_write_version(&database.pool, repo.id, Some(Some(storage.node_id)))
            .await
            .unwrap()
    );
    current_copies(&database, &[repo.id, fork_meta.repo_id], copy.node_id).await;
    assert_eq!(
        db::gists::pending(&database.pool, repo.id)
            .await
            .unwrap()
            .unwrap()
            .storage_node_id,
        Some(storage.node_id)
    );
    let moved = replicas::promote(&database.pool, repo.id, Some(copy.node_id), false)
        .await
        .unwrap();
    assert_eq!(moved.len(), 2);
    let recovered = api(
        &web,
        None,
        "gists/get",
        Method::GET,
        &path,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(recovered["description"], "Recovered after promotion");
    assert_eq!(recovered["history"][0]["version"], sha);
    assert_eq!(
        recovered["files"]["recovered.txt"]["content"],
        "published before promotion\n"
    );
    assert!(db::gists::pending(&database.pool, repo.id)
        .await
        .unwrap()
        .is_none());
    api(
        &web,
        Some(&token),
        "gists/update",
        Method::PATCH,
        &path,
        json!({"description":"Writes follow the promoted primary"}),
        StatusCode::OK,
    )
    .await;
    db::users::ssh_keys::delete_ssh_key(&database.pool, account_key.id)
        .await
        .unwrap();
    assert!(!git(root.path(), &["ls-remote", &ssh], None, &key)
        .await
        .status
        .success());
    let token_id = db::users::tokens::find_token(
        &database.pool,
        &hex::encode(Sha256::digest(token.as_bytes())),
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    db::users::tokens::delete_token(&database.pool, alice.id, session, token_id)
        .await
        .unwrap();
    assert!(!git(&work, &["push", "origin", "main"], Some(&token), &key)
        .await
        .status
        .success());
    sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=$1")
        .bind(alice.id)
        .execute(&database.raw)
        .await
        .unwrap();
    api(
        &web,
        None,
        "gists/get",
        Method::GET,
        &path,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert!(!git(root.path(), &["ls-remote", &http], None, &key)
        .await
        .status
        .success());
    sqlx::query("UPDATE users SET is_banned=FALSE WHERE id=$1")
        .bind(alice.id)
        .execute(&database.raw)
        .await
        .unwrap();
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1")
        .bind(repo.id)
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(runs, 0);
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM timeline_events WHERE repo_id=$1")
        .bind(repo.id)
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(events, 0);
    let fresh = write_token(&database.pool, alice.id).await;
    api(
        &web,
        Some(&fresh),
        "gists/delete",
        Method::DELETE,
        &path,
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    api(
        &web,
        None,
        "gists/get",
        Method::GET,
        &format!("/api/v3/gists/{fork_id}"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    success(
        git(
            root.path(),
            &["ls-remote", &format!("{}/gists/{fork_id}.git", web.base)],
            None,
            &deploy,
        )
        .await,
    );
    drop(copy);
    drop(web);
    drop(storage);
    database.finish().await;
}
