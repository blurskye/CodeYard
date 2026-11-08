//! The hourly region evaluation moves a user whose own counted requests come from another
//! region, through the same move as `cell move`, and keeps to its limits.
use crate::support::native_database::PostgresDatabase;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use codeyard::backend::cli::cells::{MoveTarget, UserMover};
use codeyard_db_placement::regions;
use codeyard_support::node_identity::{self, NodeKey};
use codeyard_types::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_types::{cell_ids::Prefix, cluster::RegionPolicy};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

/// Sets one account's counted requests from `region` on each of the last `days` days.
async fn set_counts(
    pool: &codeyard_database::pool::PgPool,
    user: i64,
    region: &str,
    requests: i64,
    days: i32,
) {
    sqlx::query("DELETE FROM cell_request_counts WHERE user_id=$1 AND region=$2")
        .bind(user)
        .bind(region)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO cell_request_counts(user_id,region,day,requests) SELECT $1,$2,(clock_timestamp() AT TIME ZONE 'UTC')::date-g,$3 FROM generate_series(0,$4-1) AS g")
        .bind(user)
        .bind(region)
        .bind(requests)
        .bind(days)
        .execute(pool)
        .await
        .unwrap();
}

/// The users audited with `action`, in order.
async fn audited(pool: &codeyard_database::pool::PgPool, action: &str) -> Vec<i64> {
    sqlx::query_scalar("SELECT target_id FROM audit_log WHERE action=$1 ORDER BY id")
        .bind(action)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn cooling(pool: &codeyard_database::pool::PgPool, user: i64) -> bool {
    sqlx::query_scalar("SELECT region_moved_at IS NOT NULL FROM users WHERE id=$1")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn hourly_evaluation_moves_a_user_to_the_region_its_requests_come_from() {
    crate::support::fake_webui_bundle::install();
    let mut source = PostgresDatabase::new().await;
    let mut target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_database::cells::initialize(&database.pool, Prefix::new(prefix))
            .await
            .unwrap();
        codeyard_database::cells::load(&database.pool)
            .await
            .unwrap();
    }
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory =
        codeyard_db_directory::Directory::connect(&registry.connection_url(), Default::default())
            .await
            .unwrap();
    directory.migrate().await.unwrap();
    let root = tempfile::tempdir().unwrap();
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
    let listener = codeyard_router::directory::Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    // Bob signs up first, so the policy reaches him before Alice.
    let bob = codeyard_db_identity::users::accounts::create_user(&source.pool, "bob", None)
        .await
        .unwrap();
    let alice = codeyard_db_identity::users::accounts::create_user(&source.pool, "alice", None)
        .await
        .unwrap();
    let disk = source_root.join(format!("{}.git", uuid::Uuid::new_v4()));
    let git = git2::Repository::init_bare(&disk).unwrap();
    let tree_id = git.treebuilder(None).unwrap().write().unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.test").unwrap();
    let commit = git
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
    let issue: i64 = sqlx::query_scalar("INSERT INTO issues(repo_id,number,author_id,title) VALUES($1,1,$2,'Follows its owner') RETURNING id")
        .bind(repo.id)
        .bind(alice.id)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    let alice_object = Object {
        kind: ObjectKind::User,
        id: alice.id,
    };
    for (object, name) in [
        (
            Object {
                kind: ObjectKind::User,
                id: bob.id,
            },
            Some("bob"),
        ),
        (alice_object, Some("alice")),
        (
            Object {
                kind: ObjectKind::Repository,
                id: repo.id,
            },
            None,
        ),
    ] {
        let keys: Vec<Key> = name
            .map(|name| Key::new(KeyKind::Name, name).unwrap())
            .into_iter()
            .collect();
        let nonce = "c".repeat(64);
        directory
            .reserve(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&source_key.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
    }
    let token = crate::move_http::token(&source, alice.id).await;
    let client = || {
        codeyard_router::directory::Client::new(
            source_key.clone(),
            listener.address,
            server_key.public_key(),
            Default::default(),
        )
        .unwrap()
    };
    source.pool.set_directory(Arc::new(client())).unwrap();
    let codeyard_database::DbPool::Postgres(pool) = &source.pool else {
        unreachable!()
    };
    let mover = UserMover {
        pool: source.pool.clone(),
        state: state.clone(),
        repositories: source_root.clone(),
        directory: client(),
        nodes: None,
        targets: [(
            2,
            MoveTarget {
                database: target.connection_url(),
                repositories: target_root.clone(),
                state: Some(target_state.clone()),
                s3: None,
                route_cache_seconds: 0,
            },
        )]
        .into(),
    };

    // The default policy is paused.
    let mut policy = RegionPolicy::default();
    assert!(mover.plan(&policy).await.unwrap().is_none());
    policy.move_paused = false;
    policy.move_daily_limit = 1;

    // Only requests that authenticate as the user count; anonymous reads never do.
    let counting = axum::Router::new()
        .fallback(|| async { "ok" })
        .layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::count_regional_requests,
        ))
        .layer(axum::Extension(Arc::new(codeyard_app::app::AppState::new(
            source.pool.clone(),
            source_root.clone(),
            Arc::new(
                codeyard_storage::ObjectStore::open(&state, None)
                    .await
                    .unwrap(),
            ),
            None,
            Default::default(),
            None,
        ))));
    for token in [None, None, Some(&token)] {
        let request = Request::builder()
            .uri("/")
            .header(codeyard_types::cells::REGION_HEADER, "b");
        let request = match token {
            Some(token) => request.header("authorization", format!("Bearer {token}")),
            None => request,
        };
        let response = counting
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    codeyard_database::cells::regions::flush(pool)
        .await
        .unwrap();
    let counted: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT user_id,region,requests FROM cell_request_counts")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(counted, [(alice.id, "b".to_owned(), 1)]);
    assert!(mover.plan(&policy).await.unwrap().is_none());

    // The share must hold on three days, and reach the policy's share.
    set_counts(pool, alice.id, "b", 1000, 2).await;
    assert!(mover.plan(&policy).await.unwrap().is_none());
    set_counts(pool, alice.id, "a", 400, 7).await;
    set_counts(pool, alice.id, "b", 600, 7).await;
    assert!(mover.plan(&policy).await.unwrap().is_none());

    // Dry run records the move it would start and changes nothing, once a day here.
    set_counts(pool, alice.id, "a", 300, 7).await;
    set_counts(pool, alice.id, "b", 700, 7).await;
    for _ in 0..2 {
        assert!(mover.plan(&policy).await.unwrap().is_none());
    }
    assert_eq!(audited(pool, "regions.user_would_move").await, [alice.id]);
    assert!(!cooling(pool, alice.id).await);
    assert_eq!(
        directory.object_home(alice_object).await.unwrap(),
        Some(Prefix::new(1))
    );

    // Live, a pinned account never moves.
    policy.move_dry_run = false;
    regions::pin_user(pool, "alice", true).await.unwrap();
    assert!(mover.plan(&policy).await.unwrap().is_none());
    regions::pin_user(pool, "alice", false).await.unwrap();

    // Bob qualifies too, but he moved a day ago, so Alice is planned.
    set_counts(pool, bob.id, "b", 2000, 7).await;
    sqlx::query("UPDATE users SET region_moved_at=clock_timestamp()-INTERVAL '1 day' WHERE id=$1")
        .bind(bob.id)
        .execute(pool)
        .await
        .unwrap();
    let planned = mover.plan(&policy).await.unwrap().unwrap();
    assert_eq!(
        (planned.user, planned.cell, planned.region.as_str()),
        (alice.id, 2, "b")
    );

    // Another move holds either cell's move lease: this one does not start, and Alice's
    // cooldown does not begin.
    for database in [&source.pool, &target.pool] {
        let lease = codeyard_db_placement::cluster::serving_lock(database, "account-move")
            .await
            .unwrap();
        assert!(mover.run(&planned).await.is_err());
        lease.release().await;
    }
    // Alice is pinned after the plan: the move does not start either.
    regions::pin_user(pool, "alice", true).await.unwrap();
    assert!(mover.run(&planned).await.unwrap().is_none());
    regions::pin_user(pool, "alice", false).await.unwrap();
    assert!(!cooling(pool, alice.id).await);
    assert!(audited(pool, "regions.user_moved").await.is_empty());
    let moving: i64 = sqlx::query_scalar("SELECT count(*) FROM account_moves")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(moving, 0);

    // The directory refuses the switch: the move stops after the copy, with Alice's
    // cooldown begun. The next evaluation continues it.
    directory
        .register_cell(cell(2, "b", &target_key, State::Disabled))
        .await
        .unwrap();
    assert!(mover.run(&planned).await.is_err());
    assert!(cooling(pool, alice.id).await);
    assert_eq!(audited(pool, "regions.user_moved").await, [alice.id]);
    directory
        .register_cell(cell(2, "b", &target_key, State::Open))
        .await
        .unwrap();
    let resumed = mover.plan(&policy).await.unwrap().unwrap();
    assert_eq!((resumed.user, resumed.cell), (alice.id, 2));
    let moved = tokio::time::timeout(Duration::from_secs(120), mover.run(&resumed))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(moved["target_cell"], 2);
    assert_eq!(moved["repositories"], 1);
    assert_eq!(
        directory.object_home(alice_object).await.unwrap(),
        Some(Prefix::new(2))
    );
    assert_eq!(audited(pool, "regions.user_moved").await, [alice.id]);
    assert!(
        cooling(&target.raw, alice.id).await,
        "the cooldown moves with the account"
    );

    // Clone, issue and repository links work from the new home.
    let clone = root.path().join("clone");
    let cloned = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("git")
            .arg("clone")
            .arg(target_root.join(disk.file_name().unwrap()))
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
        Some(commit)
    );
    codeyard_db_placement::node_config::upsert_node_configuration(&target.pool, "public", true)
        .await
        .unwrap();
    let app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        target.pool.clone(),
        target_root.clone(),
        Arc::new(
            codeyard_storage::ObjectStore::open(&target_state, None)
                .await
                .unwrap(),
        ),
        None,
        Default::default(),
        None,
    ));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v3/repos/alice/project/issues/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["id"], issue);
    assert_eq!(body["title"], "Follows its owner");

    // Bob's cooldown ends, but today's moves to region B reached the limit. With a higher
    // limit he is planned next, one move per evaluation.
    sqlx::query("UPDATE users SET region_moved_at=NULL WHERE id=$1")
        .bind(bob.id)
        .execute(pool)
        .await
        .unwrap();
    assert!(mover.plan(&policy).await.unwrap().is_none());
    policy.move_daily_limit = 2;
    assert_eq!(mover.plan(&policy).await.unwrap().unwrap().user, bob.id);

    listener.stop().await;
    directory.close().await;
    registry.finish().await;
    source.finish().await;
    target.finish().await;
}
