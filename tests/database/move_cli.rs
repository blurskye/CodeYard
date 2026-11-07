//! The operator move runs against two real cells and a pinned directory, then Git and
//! HTTP reads preserved issue/pull numbers, CI history and shared-store package bytes at the destination.
use crate::support::native_database::PostgresDatabase;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::{
    cells::Prefix,
    node_identity::{self, NodeKey},
};
use sha2::{Digest, Sha512};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

#[tokio::test]
async fn cli_move_rolls_back_a_failed_load_then_preserves_git_and_resource_ids() {
    crate::support::fake_webui_bundle::install();
    let mut source = PostgresDatabase::new().await;
    let mut target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_db_core::cells::initialize(&database.pool, Prefix::new(prefix))
            .await
            .unwrap();
        codeyard_db_core::cells::load(&database.pool).await.unwrap();
    }
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory =
        codeyard_db_directory::Directory::connect(&registry.connection_url(), Default::default())
            .await
            .unwrap();
    directory.migrate().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let source_root = root.path().join("one");
    let target_root = root.path().join("two");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    let state = root.path().join("state");
    let source_key = node_identity::load_or_create(&state).unwrap();
    let target_key = NodeKey::generate();
    let server_key = NodeKey::generate();
    let cell = |prefix: u16, key: &NodeKey, state: State| Cell {
        id: Prefix::new(prefix),
        name: format!("cell-{prefix}"),
        region: "a".into(),
        state,
        web_urls: vec![format!("http://127.0.0.1:{}/", 3000 + prefix)],
        ssh_addresses: vec![],
        rpc_addresses: vec![],
        node_keys: vec![key.public_hex()],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 1000000,
        maximum_write_rate: 100,
        read_regions: vec![],
    };
    for (prefix, key) in [(1, &source_key), (2, &target_key)] {
        directory
            .register_cell(cell(prefix, key, State::Open))
            .await
            .unwrap();
    }
    let listener = codeyard_cells_router::directory::Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let directory_address = listener.address;
    let alice = codeyard_db_identity::users::accounts::create_user(&source.pool, "alice", None)
        .await
        .unwrap();
    let alice_token = crate::move_http::token(&source, alice.id).await;
    let disk = source_root.join(format!("{}.git", uuid::Uuid::new_v4()));
    let git = git2::Repository::init_bare(&disk).unwrap();
    let tree_id = git.treebuilder(None).unwrap().write().unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    let oid = git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Before the move",
            &git.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();
    git.set_head("refs/heads/main").unwrap();
    drop(git);
    let repo = codeyard_db_repos::repositories::records::create_repository(
        &source.pool,
        alice.id,
        "project",
        None,
        false,
        disk.to_str().unwrap(),
    )
    .await
    .unwrap();
    let issue: i64 = sqlx::query_scalar("INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,7,$2,'Preserved issue') RETURNING id")
        .bind(repo.id).bind(alice.id).fetch_one(&source.raw).await.unwrap();
    let pull: i64 = sqlx::query_scalar("INSERT INTO pull_requests(repo_id,number,author_id,title,head_repo_id,head_branch,base_branch,head_sha,base_sha) VALUES($1,8,$2,'Preserved pull',$1,'main','main',$3,$3) RETURNING id")
        .bind(repo.id).bind(alice.id).bind(oid.to_string()).fetch_one(&source.raw).await.unwrap();
    let ci = codeyard_db_actions::ci::enqueue_ci_run(
        &source.pool,
        codeyard_db_actions::ci::NewCiRun {
            repo_id: repo.id,
            triggered_by_id: alice.id,
            pipeline: "ci",
            event: "push",
            pull_request_id: None,
            branch: "main",
            head_sha: &oid.to_string(),
            workflow_path: ".github/workflows/ci.yml",
            workflow_blobs: &serde_json::json!({".github/workflows/ci.yml":oid.to_string()}),
            workflow_inputs: None,
            environment: None,
            manual: false,
        },
        &[alice.id],
    )
    .await
    .unwrap()
    .id;
    sqlx::query("UPDATE ci_runs SET status='success',started_at=clock_timestamp(),completed_at=clock_timestamp() WHERE id=$1")
        .bind(ci).execute(&source.raw).await.unwrap();
    // Both cells use one store, as with a regional S3 bucket. This exercises shared
    // bytes and their copied references, not cross-region copying or S3 transport.
    let store = Arc::new(
        codeyard_storage::ObjectStore::open(root.path(), None)
            .await
            .unwrap(),
    );
    store.prepare().await.unwrap();
    let package_json = br#"{"name":"@alice/move-package","version":"1.0.0"}"#;
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(package_json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "package/package.json", package_json.as_slice())
        .unwrap();
    let package_bytes = archive.into_inner().unwrap().finish().unwrap();
    let digest = codeyard_app::packages::digest(&package_bytes);
    let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&package_bytes)));
    let key = codeyard_db_repos::packages::object_key(&digest);
    store
        .put_bytes(
            &codeyard_storage::Upload {
                key: &key,
                bytes: package_bytes.len() as i64,
                content_type: "application/octet-stream",
                sha256_hex: &digest[7..],
            },
            package_bytes.clone(),
            Arc::new(store.try_acquire_io().unwrap()),
        )
        .await
        .unwrap();
    let package: i64 = sqlx::query_scalar("INSERT INTO packages(owner_kind,owner_user_id,name,package_type,visibility,repo_id,created_at,updated_at) VALUES('user',$1,'move-package','npm','public',$2,0,0) RETURNING id")
        .bind(alice.id).bind(repo.id).fetch_one(&source.raw).await.unwrap();
    let version: i64 = sqlx::query_scalar("INSERT INTO package_versions(package_id,name,digest,media_type,manifest,created_at,updated_at) VALUES($1,'1.0.0',$2,'application/json',$3,0,0) RETURNING id")
        .bind(package).bind(&digest)
        .bind(serde_json::json!({"name":"@alice/move-package","version":"1.0.0","dist":{"integrity":integrity}}).to_string())
        .fetch_one(&source.raw).await.unwrap();
    sqlx::query("INSERT INTO package_blobs(digest,size,ready,expires_at) VALUES($1,$2,true,0)")
        .bind(&digest)
        .bind(package_bytes.len() as i64)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO package_version_blobs(version_id,digest) VALUES($1,$2)")
        .bind(version)
        .bind(&digest)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO package_tags(package_id,tag,version_id) VALUES($1,'latest',$2)")
        .bind(package)
        .bind(version)
        .execute(&source.raw)
        .await
        .unwrap();
    codeyard_db_cluster::node_config::upsert_node_configuration(&source.pool, "public", true)
        .await
        .unwrap();
    let source_app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        source.pool.clone(),
        source_root.clone(),
        store.clone(),
        None,
        Default::default(),
        None,
    ));
    let package_path = "/api/packages/alice/npm/@alice/move-package/-/move-package-1.0.0.tgz";
    let response = source_app
        .oneshot(
            Request::builder()
                .uri(package_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .as_ref(),
        package_bytes
    );
    let issue_node = codeyard_model::node_id::scoped("I", repo.id, issue).unwrap();
    let pull_node = codeyard_model::node_id::scoped("PR", repo.id, pull).unwrap();
    let objects = [
        Object {
            kind: ObjectKind::User,
            id: alice.id,
        },
        Object {
            kind: ObjectKind::Repository,
            id: repo.id,
        },
    ];
    for object in objects {
        let keys = if object.kind == ObjectKind::User {
            vec![Key::new(KeyKind::Name, "alice").unwrap()]
        } else {
            vec![]
        };
        let nonce = "a".repeat(64);
        directory
            .reserve(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
    }
    // Fail after the account/repository rows have been staged, not during preflight.
    sqlx::raw_sql("CREATE FUNCTION reject_move_issue() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected copy failure'; END $$; CREATE TRIGGER reject_move_issue BEFORE INSERT ON issues FOR EACH ROW EXECUTE FUNCTION reject_move_issue();")
        .execute(&target.raw).await.unwrap();
    let run = || {
        let mut command =
            tokio::process::Command::new(std::env::var_os("GITSERV_TEST_OPENSSH_HELPER").unwrap());
        command
            .args([
                "cell",
                "move",
                "--kind",
                "user",
                "--name",
                "alice",
                "--target-database",
                &target.connection_url(),
                "--target-repositories",
                target_root.to_str().unwrap(),
                "--route-cache-seconds",
                "0",
                "--wait-seconds",
                "10",
            ])
            .current_dir(root.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("DATABASE_URL", source.connection_url())
            .env("GITSERV_STATE_DIR", &state)
            .env("REPOS_PATH", &source_root)
            .env("DIRECTORY_ADDRESS", directory_address.to_string())
            .env("DIRECTORY_PUBLIC_KEY", server_key.public_hex())
            .kill_on_drop(true);
        command
    };
    // An incoming move must share the destination's move slot with an outgoing one.
    let destination_lease =
        codeyard_db_cluster::cluster::serving_lock(&target.pool, "account-move")
            .await
            .unwrap();
    let busy = tokio::time::timeout(Duration::from_secs(30), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!busy.status.success());
    assert!(
        String::from_utf8_lossy(&busy.stderr).contains("another server"),
        "{}",
        String::from_utf8_lossy(&busy.stderr)
    );
    assert!(std::fs::read_dir(&target_root).unwrap().next().is_none());
    destination_lease.release().await;
    let failed = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !failed.status.success(),
        "the injected target failure must fail the move"
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("injected copy failure"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    for object in objects {
        assert_eq!(
            directory.object_home(object).await.unwrap(),
            Some(Prefix::new(1))
        );
    }
    let native: bool = sqlx::query_scalar("SELECT NOT is_stub FROM users WHERE id=$1")
        .bind(alice.id)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert!(native);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories WHERE id=$1")
        .bind(repo.id)
        .fetch_one(&target.raw)
        .await
        .unwrap();
    assert_eq!(count, 0, "a failed load rolls back all target rows");
    // A retry must refuse a destination that became active before touching its Git,
    // not merely discover that conflict when it eventually loads the database rows.
    let destination = target_root.join(disk.file_name().unwrap());
    sqlx::query("INSERT INTO users(id,username,home_cell) VALUES($1,'alice',2)")
        .bind(alice.id)
        .execute(&target.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO repositories(id,owner_kind,owner_user_id,name,disk_path,home_cell) VALUES($1,'user',$2,'project',$3,2)")
        .bind(repo.id).bind(alice.id).bind(destination.to_str().unwrap())
        .execute(&target.raw).await.unwrap();
    let active_git = git2::Repository::open_bare(&destination).unwrap();
    let parent = active_git.find_commit(oid).unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    let active_oid = active_git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Destination-only write",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
    drop(parent);
    drop(active_git);
    let active = tokio::time::timeout(Duration::from_secs(30), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!active.status.success());
    assert!(
        String::from_utf8_lossy(&active.stderr).contains("target account is already active"),
        "{}",
        String::from_utf8_lossy(&active.stderr)
    );
    assert_eq!(
        git2::Repository::open_bare(&destination)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(active_oid)
    );
    let move_count: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(
        move_count, 0,
        "destination refusal must unfreeze the source"
    );
    sqlx::query("DELETE FROM repositories WHERE id=$1")
        .bind(repo.id)
        .execute(&target.raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(alice.id)
        .execute(&target.raw)
        .await
        .unwrap();
    std::fs::remove_dir_all(&destination).unwrap();
    sqlx::query("UPDATE issues SET title='Preserved issue after retry' WHERE id=$1")
        .bind(issue)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER reject_move_issue ON issues")
        .execute(&target.raw)
        .await
        .unwrap();
    // The destination can already have accepted writes when the directory refuses the
    // switch. A retry must finish switching, never replace that active copy.
    directory
        .register_cell(cell(2, &target_key, State::Disabled))
        .await
        .unwrap();
    let interrupted = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!interrupted.status.success());
    let phase: String = sqlx::query_scalar(
        "SELECT state FROM account_moves WHERE account_kind='user' AND account_id=$1",
    )
    .bind(alice.id)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    assert_eq!(
        phase,
        "switched",
        "{}",
        String::from_utf8_lossy(&interrupted.stderr)
    );
    sqlx::query("UPDATE issues SET title='Preserved issue after switch retry' WHERE id=$1")
        .bind(issue)
        .execute(&target.raw)
        .await
        .unwrap();
    directory
        .register_cell(cell(2, &target_key, State::Open))
        .await
        .unwrap();
    // Fail filesystem cleanup after the source rows have been released. Retain the
    // original directory aside and put a non-directory at its path, so remove_dir_all
    // fails deterministically even when the test runs with privileged filesystem access.
    let retained_source = source_root.join("retained-source.git");
    std::fs::rename(&disk, &retained_source).unwrap();
    std::fs::write(&disk, b"injected cleanup failure").unwrap();
    let cleanup_failure = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!cleanup_failure.status.success());
    let phase: String = sqlx::query_scalar(
        "SELECT state FROM account_moves WHERE account_kind='user' AND account_id=$1",
    )
    .bind(alice.id)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    assert_eq!(
        phase,
        "released",
        "{}",
        String::from_utf8_lossy(&cleanup_failure.stderr)
    );
    let source_stub: bool = sqlx::query_scalar("SELECT is_stub FROM users WHERE id=$1")
        .bind(alice.id)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert!(source_stub);
    for object in objects {
        assert_eq!(
            directory.object_home(object).await.unwrap(),
            Some(Prefix::new(2))
        );
    }
    // The destination continues taking writes during cleanup and must never be copied
    // again, including when the final successful reply is lost and the CLI repeats.
    let target_git = git2::Repository::open_bare(&destination).unwrap();
    let parent = target_git.find_commit(oid).unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    let final_oid = target_git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "After release",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
    drop(parent);
    drop(target_git);
    std::fs::remove_file(&disk).unwrap();
    std::fs::rename(&retained_source, &disk).unwrap();
    let unrelated = source_root.join("unrelated.git");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::write(unrelated.join("keep"), b"not moved").unwrap();
    let moved = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    for object in objects {
        assert_eq!(
            directory.object_home(object).await.unwrap(),
            Some(Prefix::new(2))
        );
    }
    assert!(
        !disk.exists(),
        "only the switched and released source Git copy is removed"
    );
    assert_eq!(std::fs::read(unrelated.join("keep")).unwrap(), b"not moved");
    let outstanding: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(
        outstanding, 0,
        "cleanup must finish before forgetting the move"
    );
    let repeated = tokio::time::timeout(Duration::from_secs(30), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    // The token keeps its source prefix. The directory sends it to the new home, the new
    // home holds it natively and the source asks the new home about it.
    let hash = codeyard_support::token_hash::token_hash(&alice_token);
    let location = directory
        .lookup(&Key::new(KeyKind::Credential, &hash).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(location.cell, Prefix::new(2));
    let moved: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM moved_credentials WHERE token_hash=$1)")
            .bind(&hash)
            .fetch_one(&source.raw)
            .await
            .unwrap();
    assert!(moved);
    let destination = target_root.join(disk.file_name().unwrap());
    let clone = root.path().join("clone");
    let cloned = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("git")
            .arg("clone")
            .arg(&destination)
            .arg(&clone)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    assert_eq!(
        git2::Repository::open(&clone)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(final_oid)
    );
    codeyard_db_cluster::node_config::upsert_node_configuration(&target.pool, "public", true)
        .await
        .unwrap();
    let app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        target.pool.clone(),
        target_root,
        store,
        None,
        Default::default(),
        None,
    ));
    let (status, viewer) = crate::move_http::request(
        &app,
        &alice_token,
        "GET",
        "/api/v3/user",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{viewer}");
    assert_eq!(viewer["login"], "alice");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v3/repos/alice/project/issues/7")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["id"], issue);
    assert_eq!(body["number"], 7);
    assert_eq!(body["title"], "Preserved issue after switch retry");
    assert_eq!(body["node_id"], issue_node);
    for (path, expected) in [
        (
            "/api/v3/repos/alice/project/pulls/8".to_owned(),
            serde_json::json!({"id":pull,"number":8,"title":"Preserved pull","node_id":pull_node}),
        ),
        (
            format!("/api/v3/repos/alice/project/actions/runs/{ci}"),
            serde_json::json!({"id":ci,"status":"completed","conclusion":"success","head_sha":oid.to_string()}),
        ),
        (
            "/api/v3/users/alice/packages/npm/move-package".to_owned(),
            serde_json::json!({"id":package,"name":"move-package","version_count":1}),
        ),
        (
            format!("/api/v3/users/alice/packages/npm/move-package/versions/{version}"),
            serde_json::json!({"id":version,"name":"1.0.0"}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        for (field, value) in expected.as_object().unwrap() {
            assert_eq!(&body[field], value, "{path}: {body}");
        }
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/packages/alice/npm/@alice/move-package")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let metadata: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(metadata["dist-tags"]["latest"], "1.0.0");
    assert_eq!(
        metadata["versions"]["1.0.0"]["dist"]["integrity"],
        integrity
    );
    let url = url::Url::parse(
        metadata["versions"]["1.0.0"]["dist"]["tarball"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        url.path(),
        package_path,
        "the pre-move package URL remains valid"
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(package_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let downloaded = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(downloaded.as_ref(), package_bytes);
    assert_eq!(codeyard_app::packages::digest(&downloaded), digest);
    // Copied blob metadata is not permission: the destination's current visibility
    // must still govern downloads using the old URL. Anonymous registry clients get the
    // same challenge for private and unknown packages, so this is 401, not 404.
    sqlx::query("UPDATE packages SET visibility='private' WHERE id=$1")
        .bind(package)
        .execute(&target.raw)
        .await
        .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(package_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    for table in [
        "issues",
        "pull_requests",
        "ci_runs",
        "packages",
        "package_versions",
        "package_version_blobs",
        "package_tags",
    ] {
        let remaining: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&source.raw)
            .await
            .unwrap();
        assert_eq!(remaining, 0, "{table} was not released from the source");
    }
    listener.stop().await;
    directory.close().await;
    registry.finish().await;
    source.finish().await;
    target.finish().await;
}

/// Bytes follow an account into another region's store: a page at a time, read back
/// before the switch, resumed after a failure, and released in the source only after the
/// switch. The target then serves the package from its own store over HTTP.
#[tokio::test]
async fn cli_move_to_another_region_copies_stored_objects_before_the_switch() {
    crate::support::fake_webui_bundle::install();
    let mut source = PostgresDatabase::new().await;
    let mut target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_db_core::cells::initialize(&database.pool, Prefix::new(prefix))
            .await
            .unwrap();
        codeyard_db_core::cells::load(&database.pool).await.unwrap();
    }
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory =
        codeyard_db_directory::Directory::connect(&registry.connection_url(), Default::default())
            .await
            .unwrap();
    directory.migrate().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let source_root = root.path().join("one");
    let target_root = root.path().join("two");
    let state = root.path().join("state");
    let target_state = root.path().join("target-state");
    for directory in [&source_root, &target_root, &target_state] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let source_key = node_identity::load_or_create(&state).unwrap();
    let target_key = NodeKey::generate();
    let server_key = NodeKey::generate();
    let cell = |prefix: u16, region: &str, key: &NodeKey, state: State| Cell {
        id: Prefix::new(prefix),
        name: format!("cell-{prefix}"),
        region: region.into(),
        state,
        web_urls: vec![format!("http://127.0.0.1:{}/", 3000 + prefix)],
        ssh_addresses: vec![],
        rpc_addresses: vec![],
        node_keys: vec![key.public_hex()],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 1000000,
        maximum_write_rate: 100,
        read_regions: vec![],
    };
    directory
        .register_cell(cell(1, "a", &source_key, State::Open))
        .await
        .unwrap();
    directory
        .register_cell(cell(2, "b", &target_key, State::Open))
        .await
        .unwrap();
    let listener = codeyard_cells_router::directory::Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let directory_address = listener.address;
    let alice = codeyard_db_identity::users::accounts::create_user(&source.pool, "alice", None)
        .await
        .unwrap();
    let disk = source_root.join(format!("{}.git", uuid::Uuid::new_v4()));
    let git = git2::Repository::init_bare(&disk).unwrap();
    let tree_id = git.treebuilder(None).unwrap().write().unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "Before the move",
        &git.find_tree(tree_id).unwrap(),
        &[],
    )
    .unwrap();
    git.set_head("refs/heads/main").unwrap();
    drop(git);
    let repo = codeyard_db_repos::repositories::records::create_repository(
        &source.pool,
        alice.id,
        "project",
        None,
        false,
        disk.to_str().unwrap(),
    )
    .await
    .unwrap();
    // Each cell keeps its files under its own state directory, as two regions' stores.
    let source_store = codeyard_storage::ObjectStore::open(&state, None)
        .await
        .unwrap();
    let put = |key: String, content_type: &'static str, bytes: Vec<u8>| {
        let store = &source_store;
        async move {
            let digest = hex::encode(<sha2::Sha256 as Digest>::digest(&bytes));
            store
                .put_bytes(
                    &codeyard_storage::Upload {
                        key: &key,
                        bytes: bytes.len() as i64,
                        content_type,
                        sha256_hex: &digest,
                    },
                    bytes.clone(),
                    Arc::new(store.try_acquire_io().unwrap()),
                )
                .await
                .unwrap();
            (key, digest, bytes)
        }
    };
    let mut picture = b"\x89PNG\r\n\x1a\n".to_vec();
    picture.extend_from_slice(b"avatar pixels");
    let (avatar_key, avatar_digest, avatar) = put(
        format!("uploads/{}", uuid::Uuid::new_v4()),
        "image/png",
        picture,
    )
    .await;
    sqlx::query("WITH f AS (INSERT INTO files(user_id,filename,original_name,content_type,file_size,sha256_hex,object_key,reason,is_active,confirmed_at) VALUES($1,'avatar.png','avatar.png','image/png',$2,$3,$4,'avatar',TRUE,clock_timestamp()) RETURNING id) UPDATE users SET avatar_file_id=(SELECT id FROM f) WHERE id=$1")
        .bind(alice.id).bind(avatar.len() as i64).bind(&avatar_digest).bind(&avatar_key)
        .execute(&source.raw).await.unwrap();
    let (part_key, _, part) = put(
        format!("actions/{}", uuid::Uuid::new_v4()),
        "application/octet-stream",
        b"artifact part".to_vec(),
    )
    .await;
    sqlx::query("WITH o AS (INSERT INTO actions_objects(repo_id,kind,scope,name,version,finalized,created_at,accessed_at,expires_at) VALUES($1,'artifact','run','logs','1',TRUE,0,0,4102444800) RETURNING id) INSERT INTO actions_blob_parts(object_id,block_id,object_key,size,ready) SELECT id,'block',$2,$3,TRUE FROM o")
        .bind(repo.id).bind(&part_key).bind(part.len() as i64)
        .execute(&source.raw).await.unwrap();
    let package_json = br#"{"name":"@alice/move-package","version":"1.0.0"}"#;
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(package_json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "package/package.json", package_json.as_slice())
        .unwrap();
    let tarball = archive.into_inner().unwrap().finish().unwrap();
    let digest = codeyard_app::packages::digest(&tarball);
    let (package_key, _, tarball) = put(
        codeyard_db_repos::packages::object_key(&digest),
        "application/octet-stream",
        tarball,
    )
    .await;
    let package: i64 = sqlx::query_scalar("INSERT INTO packages(owner_kind,owner_user_id,name,package_type,visibility,repo_id,created_at,updated_at) VALUES('user',$1,'move-package','npm','public',$2,0,0) RETURNING id")
        .bind(alice.id).bind(repo.id).fetch_one(&source.raw).await.unwrap();
    let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&tarball)));
    let version: i64 = sqlx::query_scalar("INSERT INTO package_versions(package_id,name,digest,media_type,manifest,created_at,updated_at) VALUES($1,'1.0.0',$2,'application/json',$3,0,0) RETURNING id")
        .bind(package).bind(&digest)
        .bind(serde_json::json!({"name":"@alice/move-package","version":"1.0.0","dist":{"integrity":integrity}}).to_string())
        .fetch_one(&source.raw).await.unwrap();
    sqlx::query("INSERT INTO package_blobs(digest,size,ready,expires_at) VALUES($1,$2,true,0)")
        .bind(&digest)
        .bind(tarball.len() as i64)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO package_version_blobs(version_id,digest) VALUES($1,$2)")
        .bind(version)
        .bind(&digest)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO package_tags(package_id,tag,version_id) VALUES($1,'latest',$2)")
        .bind(package)
        .bind(version)
        .execute(&source.raw)
        .await
        .unwrap();
    let user = Object {
        kind: ObjectKind::User,
        id: alice.id,
    };
    for object in [
        user,
        Object {
            kind: ObjectKind::Repository,
            id: repo.id,
        },
    ] {
        let keys = if object.kind == ObjectKind::User {
            vec![Key::new(KeyKind::Name, "alice").unwrap()]
        } else {
            vec![]
        };
        let nonce = "b".repeat(64);
        directory
            .reserve(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
    }
    let run = || {
        let mut command =
            tokio::process::Command::new(std::env::var_os("GITSERV_TEST_OPENSSH_HELPER").unwrap());
        command
            .args([
                "cell",
                "move",
                "--kind",
                "user",
                "--name",
                "alice",
                "--target-database",
                &target.connection_url(),
                "--target-repositories",
                target_root.to_str().unwrap(),
                "--target-state",
                target_state.to_str().unwrap(),
                "--object-page-size",
                "1",
                "--route-cache-seconds",
                "0",
                "--wait-seconds",
                "10",
            ])
            .current_dir(root.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("DATABASE_URL", source.connection_url())
            .env("GITSERV_STATE_DIR", &state)
            .env("REPOS_PATH", &source_root)
            .env("DIRECTORY_ADDRESS", directory_address.to_string())
            .env("DIRECTORY_PUBLIC_KEY", server_key.public_hex())
            .kill_on_drop(true);
        command
    };
    let target_store = codeyard_storage::ObjectStore::open(&target_state, None)
        .await
        .unwrap();
    let pending = || {
        let raw = &source.raw;
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT object_key FROM pending_object_deletions \
                 WHERE delete_after>clock_timestamp()+INTERVAL '6 days' ORDER BY object_key",
            )
            .fetch_all(raw)
            .await
            .unwrap()
        }
    };
    // Keys copy in order: the Actions part, the package, then the avatar. With the avatar
    // missing here the copy stops after the first two and the move is undone.
    let avatar_path = state.join("objects").join(&avatar_key);
    let aside = root.path().join("avatar-aside");
    std::fs::rename(&avatar_path, &aside).unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains(&format!("Cannot read object {avatar_key}")),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let moving: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(moving, 0, "a failed copy unfreezes the source");
    let copied: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id=$1 AND NOT is_stub")
        .bind(alice.id)
        .fetch_one(&target.raw)
        .await
        .unwrap();
    assert_eq!(copied, 0, "a failed copy leaves no account in the target");
    assert_eq!(
        directory.object_home(user).await.unwrap(),
        Some(Prefix::new(1))
    );
    assert_eq!(
        target_store
            .read_object(&part_key, part.len())
            .await
            .unwrap(),
        part
    );
    assert!(target_store
        .read_object(&avatar_key, avatar.len())
        .await
        .is_err());
    assert!(pending().await.is_empty());
    std::fs::rename(&aside, &avatar_path).unwrap();
    // The directory refuses the switch: the copy is complete and verified, but nothing
    // in the source is released yet.
    directory
        .register_cell(cell(2, "b", &target_key, State::Disabled))
        .await
        .unwrap();
    let interrupted = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!interrupted.status.success());
    let phase: String = sqlx::query_scalar(
        "SELECT state FROM account_moves WHERE account_kind='user' AND account_id=$1",
    )
    .bind(alice.id)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    assert_eq!(
        phase,
        "switched",
        "{}",
        String::from_utf8_lossy(&interrupted.stderr)
    );
    for (key, bytes) in [
        (&avatar_key, &avatar),
        (&part_key, &part),
        (&package_key, &tarball),
    ] {
        assert_eq!(
            &target_store.read_object(key, bytes.len()).await.unwrap(),
            bytes
        );
    }
    assert!(pending().await.is_empty());
    directory
        .register_cell(cell(2, "b", &target_key, State::Open))
        .await
        .unwrap();
    let moved = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    assert_eq!(
        directory.object_home(user).await.unwrap(),
        Some(Prefix::new(2))
    );
    // Uniquely named source objects go after the restore window; the package blob is left
    // to the source's package collector.
    let mut retired = vec![avatar_key.clone(), part_key.clone()];
    retired.sort();
    assert_eq!(pending().await, retired);
    assert_eq!(
        source_store
            .read_object(&avatar_key, avatar.len())
            .await
            .unwrap(),
        avatar,
        "source bytes stay until the restore window ends"
    );
    let repeated = tokio::time::timeout(Duration::from_secs(30), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    codeyard_db_cluster::node_config::upsert_node_configuration(&target.pool, "public", true)
        .await
        .unwrap();
    let app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        target.pool.clone(),
        target_root,
        Arc::new(target_store),
        None,
        Default::default(),
        None,
    ));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/packages/alice/npm/@alice/move-package/-/move-package-1.0.0.tgz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .as_ref(),
        tarball
    );
    listener.stop().await;
    directory.close().await;
    registry.finish().await;
    source.finish().await;
    target.finish().await;
}
