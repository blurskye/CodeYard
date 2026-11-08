//! A repository transferred to an owner in another cell keeps its issue and pull request
//! numbers and Git history, and its old name redirects. The CLI run fails halfway, crashes
//! after the switch and repeats; the step test races a writer against the freeze and
//! revokes a collaborator while the move runs.
use crate::move_http::{app, request, token};
use crate::support::native_database::PostgresDatabase;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard_db_placement::moves::{self, Account};
use codeyard_security::node_identity::{self, NodeKey};
use codeyard_types::cell_ids::Prefix;
use codeyard_types::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use tower::ServiceExt;

async fn cells() -> (PostgresDatabase, PostgresDatabase) {
    let source = PostgresDatabase::new().await;
    let target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_database::cells::initialize(&database.pool, Prefix::new(prefix))
            .await
            .unwrap();
        codeyard_database::cells::load(&database.pool)
            .await
            .unwrap();
    }
    (source, target)
}

/// An anonymous GET; returns the status and the Location header.
async fn get(app: &Router, path: &str) -> (StatusCode, Option<String>) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let location = response
        .headers()
        .get("location")
        .map(|value| value.to_str().unwrap().to_owned());
    (response.status(), location)
}

#[tokio::test]
async fn a_repository_transfer_freezes_only_that_repository_and_drops_former_owner_grants() {
    crate::support::fake_webui_bundle::install();
    let (mut source, mut target) = cells().await;
    let mut users = Vec::new();
    for name in ["alice", "dave", "erin"] {
        users.push(
            codeyard_db_identity::users::accounts::create_user(&source.pool, name, None)
                .await
                .unwrap()
                .id,
        );
    }
    let [alice, dave, erin] = users[..] else {
        unreachable!()
    };
    let bob = codeyard_db_identity::users::accounts::create_user(&target.pool, "bob", None)
        .await
        .unwrap()
        .id;
    let organization: i64 = sqlx::query_scalar(
        "INSERT INTO organizations(name,home_cell) VALUES('acme',1) RETURNING id",
    )
    .fetch_one(&source.raw)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_settings(org_id) VALUES($1)")
        .bind(organization)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')")
        .bind(organization)
        .bind(alice)
        .execute(&source.raw)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut repositories = Vec::new();
    for name in ["project", "other"] {
        let disk = root.path().join(format!("{name}.git"));
        git2::Repository::init_bare(&disk).unwrap();
        repositories.push(
            sqlx::query_scalar::<_, i64>("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,home_cell) VALUES('org',$1,$2,$3,1) RETURNING id")
                .bind(organization).bind(name).bind(disk.to_str().unwrap())
                .fetch_one(&source.raw).await.unwrap(),
        );
    }
    let [project, other] = repositories[..] else {
        unreachable!()
    };
    let team: i64 = sqlx::query_scalar(
        "INSERT INTO teams(org_id,name,slug) VALUES($1,'Devs','devs') RETURNING id",
    )
    .bind(organization)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    sqlx::query("INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,3)")
        .bind(team)
        .bind(project)
        .execute(&source.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO repo_access(user_id,repo_id,access_level) SELECT id,$1,3 FROM unnest($2::bigint[]) id")
        .bind(project).bind(vec![dave, erin]).execute(&source.raw).await.unwrap();
    sqlx::query("INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,7,$2,'Kept number')")
        .bind(project)
        .bind(alice)
        .execute(&source.raw)
        .await
        .unwrap();
    let alice_token = token(&source, alice).await;
    let source_app = app(&source, root.path()).await;

    let account = Account::new(ObjectKind::Repository, project).unwrap();
    assert!(moves::transfer(&source.raw, &target.raw, account, "nobody")
        .await
        .is_err());
    let transfer = moves::transfer(&source.raw, &target.raw, account, "bob")
        .await
        .unwrap()
        .unwrap();
    let wait = Duration::from_secs(10);
    moves::begin(&source.raw, account, 2, wait).await.unwrap();
    // A collaborator removed after the move started must not come back at the destination.
    sqlx::query("DELETE FROM repo_access WHERE user_id=$1 AND repo_id=$2")
        .bind(dave)
        .bind(project)
        .execute(&source.raw)
        .await
        .unwrap();
    // This fixture has no peer listeners for the outbox worker to deliver to.
    sqlx::query("UPDATE cell_outbox SET delivered_at=clock_timestamp() WHERE delivered_at IS NULL")
        .execute(&source.raw)
        .await
        .unwrap();
    moves::freeze(&source.raw, account, Duration::from_secs(300), wait)
        .await
        .unwrap();
    let (status, body) = request(
        &source_app,
        &alice_token,
        "POST",
        "/api/v3/repos/acme/project/issues",
        json!({"title":"Must not land"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, body) = request(
        &source_app,
        &alice_token,
        "POST",
        "/api/v3/repos/acme/other/issues",
        json!({"title":"The owner's other repository keeps working"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let refused = sqlx::query("UPDATE repositories SET description='late' WHERE id=$1")
        .bind(project)
        .execute(&source.raw)
        .await
        .unwrap_err();
    assert_eq!(
        refused.as_database_error().unwrap().code().as_deref(),
        Some("55000")
    );

    let paths = BTreeMap::from([(project, "/moved/project.git".to_owned())]);
    let copied = moves::copy(
        &source.raw,
        &target.raw,
        account,
        2,
        &paths,
        Some(&transfer),
    )
    .await
    .unwrap();
    assert!(copied.dropped >= 1, "the team grant points at acme's team");
    moves::switch(&source.raw, account).await.unwrap();
    moves::activate(&source.raw, &target.raw, account)
        .await
        .unwrap();
    moves::release(&source.raw, account, Some(&transfer))
        .await
        .unwrap();
    moves::release(&source.raw, account, Some(&transfer))
        .await
        .unwrap();
    moves::finish(&source.raw, account).await.unwrap();

    let moved: (String, Option<i64>, Option<i64>, bool, String) = sqlx::query_as(
        "SELECT owner_kind,owner_user_id,owner_org_id,is_stub,disk_path FROM repositories WHERE id=$1",
    )
    .bind(project)
    .fetch_one(&target.raw)
    .await
    .unwrap();
    assert_eq!(
        moved,
        (
            "user".to_owned(),
            Some(bob),
            None,
            false,
            "/moved/project.git".to_owned()
        )
    );
    let numbers: Vec<i32> = sqlx::query_scalar("SELECT number FROM issues WHERE repo_id=$1")
        .bind(project)
        .fetch_all(&target.raw)
        .await
        .unwrap();
    assert_eq!(numbers, [7]);
    let collaborators: Vec<i64> =
        sqlx::query_scalar("SELECT user_id FROM repo_access WHERE repo_id=$1 ORDER BY user_id")
            .bind(project)
            .fetch_all(&target.raw)
            .await
            .unwrap();
    assert_eq!(collaborators, [erin]);
    let teams: i64 = sqlx::query_scalar("SELECT count(*) FROM team_repo_access")
        .fetch_one(&target.raw)
        .await
        .unwrap();
    assert_eq!(teams, 0);
    let stub: (String, Option<i64>, bool, i32) = sqlx::query_as(
        "SELECT owner_kind,owner_user_id,is_stub,home_cell FROM repositories WHERE id=$1",
    )
    .bind(project)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    assert_eq!(stub, ("user".to_owned(), Some(bob), true, 2));
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM issues WHERE repo_id=$1)+(SELECT count(*) FROM repo_access WHERE repo_id=$1)+(SELECT count(*) FROM team_repo_access WHERE repo_id=$1)",
    )
    .bind(project)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    assert_eq!(left, 0);
    let native: bool = sqlx::query_scalar("SELECT NOT is_stub FROM repositories WHERE id=$1")
        .bind(other)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert!(native);

    // The old name redirects through the API and anonymous Git.
    assert_eq!(
        get(&source_app, "/api/v3/repos/acme/project/issues/7").await,
        (
            StatusCode::MOVED_PERMANENTLY,
            Some("/api/v3/repos/bob/project/issues/7".to_owned())
        )
    );
    assert_eq!(
        get(
            &source_app,
            "/acme/project.git/info/refs?service=git-upload-pack"
        )
        .await,
        (
            StatusCode::MOVED_PERMANENTLY,
            Some("/bob/project.git/info/refs?service=git-upload-pack".to_owned())
        )
    );
    // A repository created later under the old name ends the redirect.
    sqlx::query("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,home_cell) VALUES('org',$1,'project',$2,1)")
        .bind(organization).bind(root.path().join("again.git").to_str().unwrap())
        .execute(&source.raw).await.unwrap();
    assert_eq!(
        get(&source_app, "/api/v3/repos/acme/project/issues/7").await,
        (StatusCode::NOT_FOUND, None)
    );
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn cli_repository_transfer_survives_a_failed_load_and_a_failed_directory_switch() {
    crate::support::fake_webui_bundle::install();
    let (mut source, mut target) = cells().await;
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
    let listener = codeyard_router::directory::Listener::bind(
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
    let bob = codeyard_db_identity::users::accounts::create_user(&target.pool, "bob", None)
        .await
        .unwrap();
    let disk = source_root.join(format!("{}.git", uuid::Uuid::new_v4()));
    let git = git2::Repository::init_bare(&disk).unwrap();
    let tree_id = git.treebuilder(None).unwrap().write().unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    let oid = git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Before the transfer",
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
    sqlx::query("INSERT INTO pull_requests(repo_id,number,author_id,title,head_repo_id,head_branch,base_branch,head_sha,base_sha) VALUES($1,8,$2,'Preserved pull',$1,'main','main',$3,$3)")
        .bind(repo.id).bind(alice.id).bind(oid.to_string()).execute(&source.raw).await.unwrap();
    let repository = Object {
        kind: ObjectKind::Repository,
        id: repo.id,
    };
    for (database_key, object, name) in [
        (
            &source_key,
            Object {
                kind: ObjectKind::User,
                id: alice.id,
            },
            Some("alice"),
        ),
        (&source_key, repository, None),
        (
            &target_key,
            Object {
                kind: ObjectKind::User,
                id: bob.id,
            },
            Some("bob"),
        ),
    ] {
        let keys: Vec<Key> = name
            .map(|name| Key::new(KeyKind::Name, name).unwrap())
            .into_iter()
            .collect();
        let nonce = "b".repeat(64);
        directory
            .reserve(&database_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&database_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
    }
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
                "repository",
                "--name",
                "alice/project",
                "--new-owner",
                "bob",
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
    let stderr =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stderr).into_owned();
    let failed = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains("injected copy failure"),
        "{}",
        stderr(&failed)
    );
    let moving: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(moving, 0, "a failed load unfreezes the source");
    let copies: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories WHERE id=$1")
        .bind(repo.id)
        .fetch_one(&target.raw)
        .await
        .unwrap();
    assert_eq!(copies, 0, "a failed load leaves nothing in the target");
    assert_eq!(
        directory.object_home(repository).await.unwrap(),
        Some(Prefix::new(1))
    );
    sqlx::query("DROP TRIGGER reject_move_issue ON issues")
        .execute(&target.raw)
        .await
        .unwrap();
    // While the directory cannot be reached the move does not start: it needs the
    // directory to finish, so the source keeps serving.
    listener.stop().await;
    let unreachable = tokio::time::timeout(Duration::from_secs(90), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!unreachable.status.success());
    let moving: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(moving, 0, "{}", stderr(&unreachable));
    let listener = codeyard_router::directory::Listener::bind(
        directory.clone(),
        &server_key,
        directory_address,
        Default::default(),
    )
    .await
    .unwrap();
    // The directory refuses the switch; the target already takes writes.
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
        "SELECT state FROM account_moves WHERE account_kind='repository' AND account_id=$1",
    )
    .bind(repo.id)
    .fetch_one(&source.raw)
    .await
    .unwrap();
    assert_eq!(phase, "switched", "{}", stderr(&interrupted));
    sqlx::query("UPDATE issues SET title='Written at the destination' WHERE id=$1")
        .bind(issue)
        .execute(&target.raw)
        .await
        .unwrap();
    directory
        .register_cell(cell(2, &target_key, State::Open))
        .await
        .unwrap();
    for attempt in ["finishes", "repeats"] {
        let output = tokio::time::timeout(Duration::from_secs(90), run().output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success(), "{attempt}: {}", stderr(&output));
    }
    assert_eq!(
        directory.object_home(repository).await.unwrap(),
        Some(Prefix::new(2))
    );
    assert!(!disk.exists(), "the source Git copy is removed");
    let outstanding: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert_eq!(outstanding, 0);
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
    assert!(cloned.status.success(), "{}", stderr(&cloned));
    assert_eq!(
        git2::Repository::open(&clone)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(oid)
    );
    let target_app = app(&target, &target_root).await;
    for (path, number, title) in [
        (
            "/api/v3/repos/bob/project/issues/7",
            7,
            "Written at the destination",
        ),
        ("/api/v3/repos/bob/project/pulls/8", 8, "Preserved pull"),
    ] {
        let response = target_app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(body["number"], number, "{path}: {body}");
        assert_eq!(body["title"], title, "{path}: {body}");
    }
    let source_app = app(&source, &source_root).await;
    assert_eq!(
        get(&source_app, "/api/v3/repos/alice/project").await,
        (
            StatusCode::MOVED_PERMANENTLY,
            Some("/api/v3/repos/bob/project".to_owned())
        )
    );
    listener.stop().await;
    directory.close().await;
    registry.finish().await;
    source.finish().await;
    target.finish().await;
}
