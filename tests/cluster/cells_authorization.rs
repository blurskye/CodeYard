//! Home answers travel over node-key TLS; directory enrollment grants no resource authority.
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::{accounts, directory, rpc};
use codeyard_db_core::cells::authorization::{self, Proof};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey, token_hash::token_hash};
use std::sync::Arc;

fn cell(prefix: Prefix, key: &NodeKey, rpc: Vec<String>) -> Cell {
    Cell {
        id: prefix,
        name: format!("cell-{prefix}"),
        region: "a".into(),
        state: State::Open,
        web_urls: vec!["http://127.0.0.1:3000".into()],
        ssh_addresses: vec![],
        rpc_addresses: rpc,
        node_keys: vec![key.public_hex()],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 1000,
        maximum_write_rate: 100,
    }
}
#[tokio::test]
async fn foreign_account_rpc_requires_live_cell_keys_and_expires_revoked_home_answers() {
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
    let user = codeyard_db_identity::users::accounts::create_user(&one.pool, "bob", None)
        .await
        .unwrap();
    let session = codeyard_identity::sessions::fresh_id(&one.pool).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        &one.pool,
        &session,
        user.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let value = Prefix::new(1)
        .token(codeyard_support::tokens::PAT_PREFIX, &"a".repeat(64))
        .unwrap();
    let token = codeyard_db_identity::users::tokens::create_token(
        &one.pool,
        user.id,
        &session,
        "home",
        &token_hash(&value),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let ssh_key = russh::keys::PrivateKey::from(
        russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[77; 32]),
    );
    let ssh_key = codeyard_db_identity::users::ssh_keys::add_ssh_key(
        &one.pool,
        user.id,
        "cell key",
        &ssh_key.public_key().to_openssh().unwrap(),
    )
    .await
    .unwrap();
    let server_key = NodeKey::generate();
    let zero_key = NodeKey::generate();
    let one_key = NodeKey::generate();
    let router_key = NodeKey::generate();
    directory
        .register_cell(cell(Prefix::ZERO, &zero_key, vec![]))
        .await
        .unwrap();
    directory
        .register_cell(cell(Prefix::new(1), &one_key, vec![]))
        .await
        .unwrap();
    directory
        .set_router_key(&router_key.public_hex(), true)
        .await
        .unwrap();
    let object = Object {
        kind: ObjectKind::User,
        id: user.id,
    };
    let keys = [
        Key::new(KeyKind::Name, &user.username).unwrap(),
        Key::new(KeyKind::Ssh, &ssh_key.fingerprint).unwrap(),
    ];
    let nonce = "e".repeat(64);
    directory
        .reserve(&one_key.public_hex(), object, &keys, &nonce)
        .await
        .unwrap();
    directory
        .confirm(&one_key.public_hex(), object, &keys, &nonce)
        .await
        .unwrap();
    let directory_listener = directory::Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let directory_client = |key| {
        directory::Client::new(
            key,
            directory_listener.address,
            server_key.public_key(),
            Default::default(),
        )
        .unwrap()
    };
    let home = accounts::listen(
        directory_client(one_key.clone()),
        Arc::new(codeyard_identity::cells::Local {
            pool: one.pool.clone(),
        }),
        &one_key,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    directory
        .register_cell(cell(
            Prefix::new(1),
            &one_key,
            vec![home.address.to_string()],
        ))
        .await
        .unwrap();
    zero.pool.set_foreign_authorization(Arc::new(
        accounts::Remote::new(zero_key.clone(), directory_client(zero_key.clone())).unwrap(),
    ));
    let proof = Proof::Token {
        home: Prefix::new(1),
        hash: token_hash(&value),
    };
    let answer = authorization::authorize(&zero.pool, &proof)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.user.id, user.id);
    assert_eq!(answer.credential_id, Some(token.id));
    assert!(answer.write);
    assert!(
        answer.user.email.is_none() && answer.user.password_hash.is_none() && !answer.user.is_admin
    );
    assert!(
        codeyard_db_identity::users::accounts::get_user_by_id(&zero.pool, user.id)
            .await
            .unwrap()
            .unwrap()
            .is_stub
    );
    let ssh_answer = authorization::authorize(
        &zero.pool,
        &Proof::Ssh {
            fingerprint: ssh_key.fingerprint.clone(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ssh_answer.user.id, user.id);
    assert_eq!(ssh_answer.credential_id, Some(ssh_key.id));
    let seconds: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM expires_at-validated_at)::float8 FROM foreign_credentials",
    )
    .fetch_one(&zero.raw)
    .await
    .unwrap();
    assert!((0.0..60.0).contains(&seconds));
    codeyard_db_cluster::node_config::upsert_node_configuration(&zero.pool, "public", true)
        .await
        .unwrap();
    let alice = codeyard_db_identity::users::accounts::create_user(&zero.pool, "alice", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("private.git");
    git2::Repository::init_bare(&path).unwrap();
    let repository = codeyard_db_repos::repositories::records::create_repository(
        &zero.pool,
        alice.id,
        "private",
        None,
        true,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        &zero.pool,
        user.id,
        repository.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let storage = Arc::new(
        codeyard_storage::ObjectStore::open(root.path(), None)
            .await
            .unwrap(),
    );
    let app = codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        zero.pool.clone(),
        root.path().into(),
        storage,
        None,
        Default::default(),
        None,
    ));
    let mut http = super::cells_frontend::Server::start(app).await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let response = client
        .post(format!("{}/api/v3/repos/alice/private/issues", http.url()))
        .bearer_auth(&value)
        .header("x-codeyard-user", alice.id.to_string())
        .json(&serde_json::json!({"title":"from the other cell","creator_id":alice.id}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let issue: serde_json::Value = response.json().await.unwrap();
    assert_eq!(issue["user"]["login"], "bob");
    let response = client
        .get(format!(
            "{}/alice/private.git/info/refs?service=git-upload-pack",
            http.url()
        ))
        .basic_auth("bob", Some(&value))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(!response.bytes().await.unwrap().is_empty());
    // Resource-cell revocation is immediate, independent of a warm home answer.
    sqlx::query("DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2")
        .bind(repository.id)
        .bind(user.id)
        .execute(&zero.raw)
        .await
        .unwrap();
    let response = client
        .post(format!("{}/api/v3/repos/alice/private/issues", http.url()))
        .bearer_auth(&value)
        .json(&serde_json::json!({"title":"must be denied"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    codeyard_db_repos::repositories::access::grant_repo_access(
        &zero.pool,
        user.id,
        repository.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    // A read-only router, an unknown node and a wrong server pin cannot request accounts.
    for (key, pin) in [
        (router_key, one_key.public_key()),
        (NodeKey::generate(), one_key.public_key()),
        (zero_key.clone(), NodeKey::generate().public_key()),
    ] {
        let client = rpc::Client::new(
            key,
            home.address,
            pin,
            accounts::ALPN,
            rpc::Config {
                exchange_seconds: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(client
            .request::<_, serde_json::Value>(&proof)
            .await
            .is_err());
    }
    sqlx::query("DELETE FROM personal_access_tokens WHERE id=$1")
        .bind(token.id)
        .execute(&one.raw)
        .await
        .unwrap();
    assert!(authorization::authorize(&zero.pool, &proof)
        .await
        .unwrap()
        .is_some());
    sqlx::query("UPDATE foreign_credentials SET expires_at=validated_at")
        .execute(&zero.raw)
        .await
        .unwrap();
    assert!(authorization::authorize(&zero.pool, &proof)
        .await
        .unwrap()
        .is_none());
    let response = client
        .post(format!("{}/api/v3/repos/alice/private/issues", http.url()))
        .bearer_auth(&value)
        .json(&serde_json::json!({"title":"revoked at home"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE repo_id=$1")
        .bind(repository.id)
        .fetch_one(&zero.raw)
        .await
        .unwrap();
    assert_eq!(count, 1);
    drop(client);
    http.stop().await;
    // Cell-key revocation is live at the receiver, even when a route is cached.
    let replacement = NodeKey::generate();
    directory
        .register_cell(cell(Prefix::ZERO, &replacement, vec![]))
        .await
        .unwrap();
    assert!(authorization::authorize(&zero.pool, &proof).await.is_err());
    home.stop().await;
    directory_listener.stop().await;
    directory.close().await;
    one.finish().await;
    zero.finish().await;
    registry.finish().await;
}
