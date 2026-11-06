//! Home answers travel over node-key TLS; directory enrollment grants no resource authority.
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::{accounts, directory, rpc};
use codeyard_db_core::cells::authorization::{self, Home, Proof};
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
    codeyard_db_cluster::node_config::upsert_node_configuration(&zero.pool, "public", true)
        .await
        .unwrap();
    let alice = codeyard_db_identity::users::accounts::create_user(&zero.pool, "alice", None)
        .await
        .unwrap();
    let alice_session = codeyard_identity::sessions::fresh_id(&zero.pool).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        &zero.pool,
        &alice_session,
        alice.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let alice_token = Prefix::ZERO
        .token(codeyard_support::tokens::PAT_PREFIX, &"b".repeat(64))
        .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        &zero.pool,
        alice.id,
        &alice_session,
        "owner",
        &token_hash(&alice_token),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard_repos_core::repositories::lifecycle::provision(
        (&zero.pool, None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(alice.id),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let organization = codeyard_db_orgs::organizations::profiles::create_organization(
        &zero.pool,
        alice.id,
        &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
            "pointer-org",
            codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                .unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(organization) =
        organization
    else {
        panic!("native organization creation refused");
    };
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
    assert!(
        codeyard_db_identity::users::accounts::get_user_by_id(&zero.pool, user.id)
            .await
            .unwrap()
            .is_none()
    );
    let response = client
        .put(format!(
            "{}/api/v3/repos/alice/private/collaborators/bob",
            http.url()
        ))
        .bearer_auth(&alice_token)
        .json(&serde_json::json!({"permission":"push"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let invitation: serde_json::Value = response.json().await.unwrap();
    assert_eq!(invitation["invitee"]["login"], "bob");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM foreign_credentials")
            .fetch_one(&zero.raw)
            .await
            .unwrap(),
        0
    );
    let response = client
        .patch(format!(
            "{}/api/v3/user/repository_invitations/{}",
            http.url(),
            invitation["id"]
        ))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    zero.pool
        .set_directory(Arc::new(directory_client(zero_key.clone())))
        .unwrap();
    let bob_root = tempfile::tempdir().unwrap();
    codeyard_db_cluster::node_config::upsert_node_configuration(&one.pool, "public", true)
        .await
        .unwrap();
    let mut bob_http = super::cells_frontend::Server::start(
        crate::support::database::router(&one.pool, bob_root.path()).await,
    )
    .await;
    let mut zero_record = cell(Prefix::ZERO, &zero_key, vec![]);
    zero_record.web_urls = vec![http.url()];
    directory.register_cell(zero_record).await.unwrap();
    let native_repository = codeyard_repos_core::repositories::lifecycle::provision(
        (&one.pool, None),
        bob_root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(user.id),
        "native",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let mut one_record = cell(Prefix::new(1), &one_key, vec![home.address.to_string()]);
    one_record.web_urls = vec![bob_http.url()];
    directory.register_cell(one_record).await.unwrap();
    one.pool.set_foreign_authorization(Arc::new(
        accounts::Remote::new(one_key.clone(), directory_client(one_key.clone())).unwrap(),
    ));
    let pointer_event: i64 = sqlx::query_scalar(
        "SELECT id FROM cell_outbox WHERE topic='membership' ORDER BY id LIMIT 1",
    )
    .fetch_one(&zero.raw)
    .await
    .unwrap();
    assert!(codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
        .await
        .unwrap());
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM remote_memberships WHERE user_id=$1 AND resource_id=$2 AND active")
        .bind(user.id).bind(repository.id).fetch_one(&one.raw).await.unwrap(),1);
    let response = client
        .get(format!("{}/api/v3/user/repos", bob_http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let listed: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{listed}");
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|repo| repo["id"] == repository.id),
        "{listed}"
    );
    let browser: serde_json::Value = client
        .get(format!("{}/", bob_http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("accept", "application/vnd.codeyard.page+json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(browser["page"]["lane"], "Discovery", "{browser}");
    assert_eq!(
        browser["page"]["model"]["Home"]["shared_count"], 1,
        "{browser}"
    );
    assert_eq!(
        browser["page"]["model"]["Home"]["shared"][0]["repository"]["id"],
        repository.id
    );
    assert_eq!(
        browser["page"]["model"]["Home"]["shared"][0]["repository"]["owner"],
        "alice"
    );
    let html = client
        .get(format!("{}/", bob_http.url()))
        .header("cookie", format!("session_id={session}"))
        .send()
        .await
        .unwrap();
    let status = html.status();
    let html = html.text().await.unwrap();
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("/alice/private"),
        "The server-rendered dashboard includes the remote repository"
    );
    let carol = codeyard_db_identity::users::accounts::create_user(&one.pool, "carol", None)
        .await
        .unwrap();
    assert!(
        codeyard_db_identity::users::accounts::get_user_by_id(&zero.pool, carol.id)
            .await
            .unwrap()
            .is_none()
    );
    let response = client
        .post(format!(
            "{}/api/v3/orgs/pointer-org/invitations",
            http.url()
        ))
        .bearer_auth(&alice_token)
        .json(&serde_json::json!({"invitee_id":carol.id}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let invited: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 201, "{invited}");
    assert_eq!(invited["login"], "carol");
    let retained = codeyard_db_identity::users::accounts::get_user_by_id(&zero.pool, carol.id)
        .await
        .unwrap()
        .unwrap();
    assert!(retained.is_stub && retained.password_hash.is_none() && retained.email.is_none());
    let response = client
        .post(format!(
            "{}/api/v3/orgs/pointer-org/invitations",
            http.url()
        ))
        .bearer_auth(&alice_token)
        .json(&serde_json::json!({"invitee_id":-1}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    for _ in 0..8 {
        if !codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
            .await
            .unwrap()
        {
            break;
        }
    }
    let response = client
        .put(format!(
            "{}/api/v3/orgs/pointer-org/memberships/bob",
            http.url()
        ))
        .bearer_auth(&alice_token)
        .json(&serde_json::json!({"role":"member"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let response = client
        .patch(format!(
            "{}/api/v3/user/memberships/orgs/pointer-org",
            http.url()
        ))
        .bearer_auth(&value)
        .json(&serde_json::json!({"state":"active"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
        .await
        .unwrap());
    let response = client
        .get(format!("{}/api/v3/user/orgs", bob_http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let orgs: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{orgs}");
    assert!(
        orgs.as_array()
            .unwrap()
            .iter()
            .any(|org| org["id"] == organization.id),
        "{orgs}"
    );
    let graphql_orgs = serde_json::json!({"query":"{viewer{organizations(first:100){totalCount nodes{id login viewerCanCreateProjects}}}}"});
    let listed: serde_json::Value = client
        .post(format!("{}/api/graphql", bob_http.url()))
        .bearer_auth(&value)
        .json(&graphql_orgs)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed.get("errors").is_none(), "{listed}");
    assert_eq!(
        listed["data"]["viewer"]["organizations"]["totalCount"], 1,
        "{listed}"
    );
    assert_eq!(
        listed["data"]["viewer"]["organizations"]["nodes"][0]["login"],
        "pointer-org"
    );
    assert!(
        listed["data"]["viewer"]["organizations"]["nodes"][0]["viewerCanCreateProjects"]
            .is_boolean()
    );
    let paged: serde_json::Value = client
        .post(format!("{}/api/graphql", bob_http.url()))
        .bearer_auth(&value)
        .json(&serde_json::json!({"query":"{viewer{empty:organizations(first:0){totalCount nodes{id}} sorted:organizations(first:1,orderBy:{field:LOGIN,direction:DESC}){totalCount nodes{login} pageInfo{hasNextPage}}}}"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(paged.get("errors").is_none(), "{paged}");
    assert_eq!(paged["data"]["viewer"]["empty"]["totalCount"], 1, "{paged}");
    assert_eq!(
        paged["data"]["viewer"]["empty"]["nodes"],
        serde_json::json!([]),
        "{paged}"
    );
    assert_eq!(
        paged["data"]["viewer"]["sorted"]["nodes"][0]["login"], "pointer-org",
        "{paged}"
    );
    assert_eq!(
        paged["data"]["viewer"]["sorted"]["pageInfo"]["hasNextPage"], false,
        "{paged}"
    );
    let browser: serde_json::Value = client
        .get(format!("{}/settings/organizations", bob_http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("accept", "application/vnd.codeyard.page+json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(browser["page"]["lane"], "Organizations", "{browser}");
    assert_eq!(
        browser["page"]["model"]["organizations"][0]["id"], organization.id,
        "{browser}"
    );
    let members: serde_json::Value = client
        .get(format!("{}/api/v3/orgs/pointer-org/members", http.url()))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(members.as_array().unwrap().len(), 1, "{members}");
    assert_eq!(members[0]["login"], "alice");
    let response = client
        .delete(format!(
            "{}/api/v3/orgs/pointer-org/memberships/bob",
            http.url()
        ))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert!(codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
        .await
        .unwrap());
    let orgs: serde_json::Value = client
        .get(format!("{}/api/v3/user/orgs", bob_http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        orgs.as_array()
            .unwrap()
            .iter()
            .all(|org| org["id"] != organization.id),
        "{orgs}"
    );
    let listed: serde_json::Value = client
        .post(format!("{}/api/graphql", bob_http.url()))
        .bearer_auth(&value)
        .json(&graphql_orgs)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed.get("errors").is_none(), "{listed}");
    assert_eq!(listed["data"]["viewer"]["organizations"]["totalCount"], 0);
    let browser: serde_json::Value = client
        .get(format!("{}/settings/organizations", bob_http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("accept", "application/vnd.codeyard.page+json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        browser["page"]["model"]["organizations"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{browser}"
    );
    for (page, expected) in [(1, repository.id), (2, native_repository.id)] {
        let response = client
            .get(format!(
                "{}/api/v3/user/repos?per_page=1&page={page}",
                bob_http.url()
            ))
            .bearer_auth(&value)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let rows: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{rows}");
        assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
        assert_eq!(rows[0]["id"], expected, "{rows}");
    }
    let mut replay = codeyard_db_core::cells::memberships::Event {
        id: pointer_event,
        change: codeyard_db_core::cells::memberships::Change {
            user_id: user.id,
            object: Object {
                kind: ObjectKind::Repository,
                id: repository.id,
            },
            active: true,
        },
    };
    assert!(
        accounts::Remote::new(router_key.clone(), directory_client(router_key.clone()))
            .unwrap()
            .send_membership(&replay)
            .await
            .is_err()
    );
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
    // A browser from the home cell uses its own synchronizer at the resource cell.
    let session_answer = authorization::authorize(
        &zero.pool,
        &Proof::Session {
            home: Prefix::new(1),
            value: session.clone(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let authorization::Authority::Session { csrf } = session_answer.authority else {
        panic!("session authority")
    };
    let response = client
        .post(format!("{}/api/repos/alice/private/issues", http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("origin", http.url())
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!({"title":"foreign browser"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions WHERE user_id=$1")
            .bind(user.id)
            .fetch_one(&zero.raw)
            .await
            .unwrap(),
        0
    );
    codeyard_db_identity::users::sessions::delete_session(&one.pool, &session)
        .await
        .unwrap();
    sqlx::query("UPDATE foreign_credentials SET expires_at=validated_at WHERE kind='session'")
        .execute(&zero.raw)
        .await
        .unwrap();
    let response = client
        .post(format!("{}/api/repos/alice/private/issues", http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("origin", http.url())
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!({"title":"revoked home browser"}))
        .send()
        .await
        .unwrap();
    assert!(matches!(response.status().as_u16(), 401 | 403));
    let browser: serde_json::Value = client
        .get(format!("{}/", bob_http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("accept", "application/vnd.codeyard.page+json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        browser["page"]["model"]["Home"]["shared"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{browser}"
    );
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
    // A stale home pointer cannot disclose the revoked resource while cleanup waits.
    let listed: serde_json::Value = client
        .get(format!("{}/api/v3/user/repos", bob_http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|repo| repo["id"] != repository.id),
        "{listed}"
    );
    assert!(codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
        .await
        .unwrap());
    zero.pool
        .foreign_authorization()
        .unwrap()
        .send_membership(&replay)
        .await
        .unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM remote_memberships WHERE user_id=$1 AND resource_id=$2 AND active")
        .bind(user.id).bind(repository.id).fetch_one(&one.raw).await.unwrap(),0);
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
    assert_eq!(count, 2);
    drop(client);
    http.stop().await;
    // Cell-key revocation is live at the receiver, even when a route is cached.
    let replacement = NodeKey::generate();
    directory
        .register_cell(cell(Prefix::ZERO, &replacement, vec![]))
        .await
        .unwrap();
    assert!(authorization::authorize(&zero.pool, &proof).await.is_err());
    assert!(zero
        .pool
        .foreign_authorization()
        .unwrap()
        .user("bob")
        .await
        .is_err());
    replay.id += 100;
    assert!(zero
        .pool
        .foreign_authorization()
        .unwrap()
        .send_membership(&replay)
        .await
        .is_err());
    bob_http.stop().await;
    home.stop().await;
    directory_listener.stop().await;
    directory.close().await;
    one.finish().await;
    zero.finish().await;
    registry.finish().await;
}
