//! Native mutations publish through real pinned directory TLS and retain local transaction authority.
use super::cells_frontend::{native, Server};
use crate::support::native_database::{NativeDatabase, PostgresDatabase};
use codeyard_cells_router::directory::{Client, Listener};
use codeyard_db_core::{cells::directory as publication, DbPool};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, Key, KeyKind, Location, Object, ObjectKind, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn cell(prefix: Prefix, key: &NodeKey, server: &Server) -> Cell {
    Cell {
        id: prefix,
        name: format!("cell-{prefix}"),
        region: "a".into(),
        state: State::Open,
        web_urls: vec![server.url()],
        ssh_addresses: vec![],
        rpc_addresses: vec![],
        node_keys: vec![key.public_hex()],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 4_000_000_000_000,
        maximum_write_rate: 10_000,
    }
}
async fn published_objects(directory: &PostgresDatabase, object: Object) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM directory_objects WHERE kind=$1 AND id=$2 AND active")
        .bind(object.kind.as_str())
        .bind(object.id)
        .fetch_one(&directory.raw)
        .await
        .unwrap()
}

async fn signup(
    client: &reqwest::Client,
    server: &Server,
    pool: &DbPool,
    name: &str,
    email: &str,
) -> reqwest::Response {
    let session = codeyard_identity::sessions::fresh_id(pool).unwrap();
    codeyard_db_identity::users::sessions::create_browser_session(
        pool,
        &session,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let csrf = crate::support::native_database::token();
    codeyard_db_identity::users::sessions::get_or_create_csrf_token(
        pool, &session, None, &csrf, 3600,
    )
    .await
    .unwrap()
    .unwrap();
    let cookie = codeyard_api_http::session::build_session_cookie(&session, 1).unwrap();
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.extend_pairs([
        ("username", name),
        ("email", email),
        ("password", "a-long-test-password"),
        ("confirm_password", "a-long-test-password"),
        ("csrf_token", csrf.as_str()),
    ]);
    client
        .post(format!("{}/register", server.url()))
        .header("cookie", cookie.split(';').next().unwrap())
        .header("origin", server.url())
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form.finish())
        .send()
        .await
        .unwrap()
}
async fn api(
    client: &reqwest::Client,
    server: &Server,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
    expected: u16,
) -> Value {
    let response = client
        .request(method, format!("{}{path}", server.url()))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    assert_eq!(
        status.as_u16(),
        expected,
        "{path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

struct LostConfirmation {
    client: Client,
    lose: AtomicBool,
}
impl publication::Directory for LostConfirmation {
    fn config(&self) -> publication::Config {
        publication::Directory::config(&self.client)
    }
    fn lookup<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, anyhow::Result<Option<Location>>> {
        publication::Directory::lookup(&self.client, key)
    }
    fn reserve<'a>(
        &'a self,
        object: Object,
        keys: &'a [Key],
        nonce: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        publication::Directory::reserve(&self.client, object, keys, nonce)
    }
    fn confirm<'a>(
        &'a self,
        object: Object,
        keys: &'a [Key],
        nonce: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            publication::Directory::confirm(&self.client, object, keys, nonce).await?;
            anyhow::ensure!(
                !self.lose.swap(false, Ordering::SeqCst),
                publication::Refusal::Unavailable
            );
            Ok(())
        })
    }
    fn release<'a>(&'a self, object: Object, keys: &'a [Key]) -> BoxFuture<'a, anyhow::Result<()>> {
        publication::Directory::release(&self.client, object, keys)
    }
    fn forget(&self, object: Object) -> BoxFuture<'_, anyhow::Result<()>> {
        publication::Directory::forget(&self.client, object)
    }
}

#[tokio::test]
async fn native_claims_refuse_cross_cell_signup_email_and_ssh_duplicates_and_retry_lost_replies() {
    crate::support::fake_webui_bundle::install();
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let mut zero = PostgresDatabase::new().await;
    let mut one = PostgresDatabase::new().await;
    codeyard_db_core::cells::initialize(&one.pool, Prefix::new(1))
        .await
        .unwrap();
    let (mut alice, alice_id, alice_token) = native(&zero, "alice").await;
    let (mut bob, _, bob_token) = native(&one, "bob").await;
    let zero_key = NodeKey::generate();
    let one_key = NodeKey::generate();
    let server_key = NodeKey::generate();
    directory
        .register_cell(cell(Prefix::ZERO, &zero_key, &alice))
        .await
        .unwrap();
    directory
        .register_cell(cell(Prefix::new(1), &one_key, &bob))
        .await
        .unwrap();
    let mut listener = Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let directory_address = listener.address;
    let connect = |key| {
        Client::new(
            key,
            directory_address,
            server_key.public_key(),
            Default::default(),
        )
        .unwrap()
    };
    let zero_client = connect(zero_key.clone());
    zero.pool
        .set_directory(Arc::new(zero_client.clone()))
        .unwrap();
    one.pool
        .set_directory(Arc::new(connect(one_key.clone())))
        .unwrap();
    let (stop_capacity, _) = tokio::sync::watch::channel(false);
    let mut capacity_workers = Vec::new();
    for (database, key) in [(&zero, &zero_key), (&one, &one_key)] {
        capacity_workers.push(tokio::spawn(codeyard_cells_router::capacity::run(
            database.pool.clone(),
            connect(key.clone()),
            Default::default(),
            stop_capacity.subscribe(),
        )));
    }
    for database in [&zero, &one] {
        let DbPool::Postgres(pool) = &database.pool else {
            unreachable!()
        };
        for kind in [ObjectKind::User, ObjectKind::Repository] {
            assert_eq!(
                publication::publish_native(pool, kind, 0, 250)
                    .await
                    .unwrap()
                    .1,
                1
            );
            assert_eq!(
                publication::publish_native(pool, kind, 0, 250)
                    .await
                    .unwrap()
                    .1,
                1,
                "publication is idempotent"
            );
        }
    }
    let operator = tempfile::tempdir().unwrap();
    std::fs::write(operator.path().join(".env"), "").unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .args([
                "cell",
                "publish",
                "--directory-address",
                &listener.address.to_string(),
                "--directory-key",
                &server_key.public_hex(),
                "--kind",
                "user",
                "--limit",
                "1",
            ])
            .env_clear()
            .env("DATABASE_URL", zero.connection_url())
            .env("NODE_KEY", zero_key.secret_hex())
            .env("GITSERV_STATE_DIR", operator.path().join("data"))
            .current_dir(operator.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let published: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(published["count"], 1);
    assert_eq!(published["after"], alice_id);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let response = signup(&client, &alice, &zero.pool, "new-user", "new@example.test").await;
    assert_eq!(
        response.status().as_u16(),
        303,
        "{}",
        response.text().await.unwrap()
    );
    for (name, email) in [
        ("new-user", "other@example.test"),
        ("other-user", "new@example.test"),
    ] {
        let response = signup(&client, &bob, &one.pool, name, email).await;
        assert_eq!(
            response.status().as_u16(),
            409,
            "{}",
            response.text().await.unwrap()
        );
        assert!(
            codeyard_db_identity::users::accounts::get_user_by_username(&one.pool, name)
                .await
                .unwrap()
                .is_none()
        );
    }
    let profile =
        codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(Some("Company"), None)
            .unwrap();
    let organization = codeyard_db_orgs::organizations::profiles::NewOrganization::new(
        "new-user",
        profile.clone(),
    )
    .unwrap();
    assert!(matches!(
        codeyard_db_orgs::organizations::profiles::create_organization(
            &one.pool,
            codeyard_db_identity::users::accounts::get_user_by_username(&one.pool, "bob")
                .await
                .unwrap()
                .unwrap()
                .id,
            &organization
        )
        .await
        .unwrap(),
        codeyard_db_core::queries::organizations::OrganizationOutcome::Conflict
    ));
    let organization =
        codeyard_db_orgs::organizations::profiles::NewOrganization::new("company", profile)
            .unwrap();
    let org = match codeyard_db_orgs::organizations::profiles::create_organization(
        &zero.pool,
        alice_id,
        &organization,
    )
    .await
    .unwrap()
    {
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) => org,
        other => panic!("{other:?}"),
    };
    let response = signup(&client, &bob, &one.pool, "company", "company@example.test").await;
    assert_eq!(response.status().as_u16(), 409);
    assert!(matches!(
        codeyard_db_orgs::organizations::deletion::delete_organization(
            &zero.pool, alice_id, org.id
        )
        .await
        .unwrap(),
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(())
    ));
    let response = signup(&client, &bob, &one.pool, "company", "company@example.test").await;
    assert_eq!(
        response.status().as_u16(),
        303,
        "{}",
        response.text().await.unwrap()
    );

    let repository = api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::POST,
        "/api/v3/user/repos",
        json!({"name":"published", "private":true}),
        201,
    )
    .await;
    let repository_object = Object {
        kind: ObjectKind::Repository,
        id: repository["id"].as_i64().unwrap(),
    };
    assert_eq!(published_objects(&registry, repository_object).await, 1);
    api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::DELETE,
        "/api/v3/repos/alice/published",
        Value::Null,
        204,
    )
    .await;
    assert_eq!(published_objects(&registry, repository_object).await, 0);

    let public_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    let key_input = json!({"title":"laptop", "key":public_key});
    let key = api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::POST,
        "/api/v3/user/keys",
        key_input.clone(),
        201,
    )
    .await;
    api(
        &client,
        &bob,
        &bob_token,
        reqwest::Method::POST,
        "/api/v3/user/keys",
        key_input.clone(),
        422,
    )
    .await;
    api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::POST,
        "/api/v3/repos/alice/repo/keys",
        key_input.clone(),
        422,
    )
    .await;
    api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::DELETE,
        &format!("/api/v3/user/keys/{}", key["id"]),
        Value::Null,
        204,
    )
    .await;
    let deploy = api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::POST,
        "/api/v3/repos/alice/repo/keys",
        key_input.clone(),
        201,
    )
    .await;
    api(
        &client,
        &bob,
        &bob_token,
        reqwest::Method::POST,
        "/api/v3/user/keys",
        key_input.clone(),
        422,
    )
    .await;
    api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::DELETE,
        &format!("/api/v3/repos/alice/repo/keys/{}", deploy["id"]),
        Value::Null,
        204,
    )
    .await;
    let bob_key = api(
        &client,
        &bob,
        &bob_token,
        reqwest::Method::POST,
        "/api/v3/user/keys",
        key_input,
        201,
    )
    .await;
    api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::POST,
        "/api/v3/user/emails",
        json!(["secondary@example.test"]),
        201,
    )
    .await;
    let response = signup(
        &client,
        &bob,
        &one.pool,
        "secondary",
        "secondary@example.test",
    )
    .await;
    assert_eq!(response.status().as_u16(), 409);
    api(
        &client,
        &alice,
        &alice_token,
        reqwest::Method::DELETE,
        "/api/v3/user/emails",
        json!(["secondary@example.test"]),
        204,
    )
    .await;
    let response = signup(
        &client,
        &bob,
        &one.pool,
        "secondary",
        "secondary@example.test",
    )
    .await;
    assert_eq!(response.status().as_u16(), 303);

    // A committed deletion keeps its cleanup when the directory is down. A fresh
    // pool resumes the same event; successful delivery can safely repeat.
    listener.stop().await;
    api(
        &client,
        &bob,
        &bob_token,
        reqwest::Method::DELETE,
        &format!("/api/v3/user/keys/{}", bob_key["id"]),
        Value::Null,
        204,
    )
    .await;
    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM cell_outbox WHERE delivered_at IS NULL")
            .fetch_one(&one.raw)
            .await
            .unwrap();
    assert!(pending > 0);
    assert!(codeyard_db_core::cells::outbox::deliver_one(&one.pool)
        .await
        .unwrap());
    listener = Listener::bind(
        directory.clone(),
        &server_key,
        directory_address,
        Default::default(),
    )
    .await
    .unwrap();
    let resumed = codeyard_db_core::init_db(&one.connection_url())
        .await
        .unwrap();
    codeyard_db_core::run_migrations(&resumed).await.unwrap();
    resumed
        .set_directory(Arc::new(
            Client::new(
                one_key.clone(),
                directory_address,
                server_key.public_key(),
                Default::default(),
            )
            .unwrap(),
        ))
        .unwrap();
    sqlx::query("UPDATE cell_outbox SET next_attempt_at=clock_timestamp()")
        .execute(&one.raw)
        .await
        .unwrap();
    for _ in 0..64 {
        if !codeyard_db_core::cells::outbox::deliver_one(&resumed)
            .await
            .unwrap()
        {
            break;
        }
    }
    let fingerprint =
        codeyard_db_identity::users::ssh_keys::calculate_fingerprint(public_key).unwrap();
    assert_eq!(
        directory
            .lookup(&Key::new(KeyKind::Ssh, &fingerprint).unwrap())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cell_outbox WHERE delivered_at IS NULL")
            .fetch_one(&one.raw)
            .await
            .unwrap(),
        0
    );
    assert!(!codeyard_db_core::cells::outbox::deliver_one(&resumed)
        .await
        .unwrap());
    resumed.close().await;

    zero.pool
        .set_directory(Arc::new(LostConfirmation {
            client: zero_client.clone(),
            lose: AtomicBool::new(true),
        }))
        .unwrap();
    assert!(
        codeyard_db_identity::users::accounts::create_user(&zero.pool, "retry-user", None)
            .await
            .is_err()
    );
    assert!(
        codeyard_db_identity::users::accounts::get_user_by_username(&zero.pool, "retry-user")
            .await
            .unwrap()
            .is_none()
    );
    let name = Key::new(KeyKind::Name, "retry-user").unwrap();
    let previous = directory.lookup(&name).await.unwrap().unwrap();
    let retried =
        codeyard_db_identity::users::accounts::create_user(&zero.pool, "retry-user", None)
            .await
            .unwrap();
    assert_ne!(
        previous.object.id, retried.id,
        "PostgreSQL never reuses a rolled-back object ID"
    );
    assert_eq!(
        directory.lookup(&name).await.unwrap().unwrap().object.id,
        retried.id
    );
    assert_eq!(published_objects(&registry, previous.object).await, 0);
    zero.pool.set_directory(Arc::new(zero_client)).unwrap();
    let mut revoked = directory.cell(Prefix::ZERO).await.unwrap().unwrap();
    revoked.node_keys = vec![NodeKey::generate().public_hex()];
    directory.register_cell(revoked).await.unwrap();
    let response = signup(
        &client,
        &alice,
        &zero.pool,
        "revoked-user",
        "revoked@example.test",
    )
    .await;
    assert_eq!(response.status().as_u16(), 503);
    assert!(codeyard_db_identity::users::accounts::get_user_by_username(
        &zero.pool,
        "revoked-user"
    )
    .await
    .unwrap()
    .is_none());
    listener.stop().await;
    let response = signup(
        &client,
        &bob,
        &one.pool,
        "outage-user",
        "outage@example.test",
    )
    .await;
    assert_eq!(response.status().as_u16(), 503);
    assert!(
        codeyard_db_identity::users::accounts::get_user_by_username(&one.pool, "outage-user")
            .await
            .unwrap()
            .is_none()
    );
    api(
        &client,
        &bob,
        &bob_token,
        reqwest::Method::GET,
        "/api/v3/user",
        Value::Null,
        200,
    )
    .await;
    stop_capacity.send_replace(true);
    for worker in capacity_workers {
        tokio::time::timeout(std::time::Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap();
    }
    alice.stop().await;
    bob.stop().await;
    directory.close().await;
    zero.finish().await;
    one.finish().await;
    registry.finish().await;
}

#[tokio::test]
async fn sqlite_stays_standalone_and_refuses_directory_enrollment() {
    let sqlite = NativeDatabase::new().await;
    let client = Client::new(
        NodeKey::generate(),
        "127.0.0.1:9".parse().unwrap(),
        NodeKey::generate().public_key(),
        Default::default(),
    )
    .unwrap();
    assert!(sqlite.pool.set_directory(Arc::new(client)).is_err());
    let user = codeyard_db_identity::users::accounts::create_user(&sqlite.pool, "standalone", None)
        .await
        .unwrap();
    assert_eq!(user.home_cell, 0);
    sqlite.close().await;
}

#[tokio::test]
async fn capacity_reports_restore_fresh_placement_and_revoked_cells_cannot_refresh_it() {
    let mut registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let mut native = PostgresDatabase::new().await;
    let member = NodeKey::generate();
    let server = NodeKey::generate();
    directory
        .register_cell(Cell {
            id: Prefix::ZERO,
            name: "capacity".into(),
            region: "a".into(),
            state: State::Open,
            web_urls: vec!["http://127.0.0.1:3000".into()],
            ssh_addresses: vec![],
            rpc_addresses: vec![],
            node_keys: vec![member.public_hex()],
            database_bytes: 0,
            write_rate: 0,
            maximum_database_bytes: 4_000_000_000_000,
            maximum_write_rate: 10_000,
        })
        .await
        .unwrap();
    sqlx::query("UPDATE directory_cells SET observed_at=clock_timestamp()-INTERVAL '1 hour'")
        .execute(&registry.raw)
        .await
        .unwrap();
    assert!(directory.choose_cell("a").await.unwrap().is_none());
    let listener = Listener::bind(
        directory.clone(),
        &server,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let client = Client::new(
        member,
        listener.address,
        server.public_key(),
        Default::default(),
    )
    .unwrap();
    assert!(
        client
            .request(&codeyard_cells_router::directory::Request::Reserve {
                object: Object {
                    kind: ObjectKind::User,
                    id: 1
                },
                keys: vec![Key::new(KeyKind::Name, "stale-user").unwrap()],
                nonce: "d".repeat(64),
            })
            .await
            .is_err(),
        "a stale capacity answer cannot admit a new account"
    );
    let DbPool::Postgres(pool) = &native.pool else {
        unreachable!()
    };
    assert!(
        codeyard_db_core::cells::capacity::sample(pool, 1)
            .await
            .is_err(),
        "catalog reads are bounded"
    );
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(codeyard_cells_router::capacity::run(
        native.pool.clone(),
        client.clone(),
        codeyard_cells_router::capacity::Config {
            sample_seconds: 1,
            ..Default::default()
        },
        stopped,
    ));
    let selected = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(cell) = directory.choose_cell("a").await.unwrap() {
                break cell;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(selected.id, Prefix::ZERO);
    assert!(selected.database_bytes > 0);
    stop.send_replace(true);
    tokio::time::timeout(std::time::Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap();
    let mut revoked = selected;
    revoked.node_keys = vec![NodeKey::generate().public_hex()];
    directory.register_cell(revoked).await.unwrap();
    sqlx::query("UPDATE directory_cells SET observed_at=clock_timestamp()-INTERVAL '1 hour'")
        .execute(&registry.raw)
        .await
        .unwrap();
    assert!(client
        .request(&codeyard_cells_router::directory::Request::ReportLoad {
            database_bytes: 0,
            write_rate: 0
        })
        .await
        .is_err());
    assert!(directory.choose_cell("a").await.unwrap().is_none());
    listener.stop().await;
    directory.close().await;
    native.finish().await;
    registry.finish().await;
}
