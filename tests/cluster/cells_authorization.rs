//! Home answers travel over node-key TLS; directory enrollment grants no resource authority.
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::{accounts, directory, rpc};
use codeyard_db_core::cells::authorization::{self, Home, Proof};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey, token_hash::token_hash};
use std::sync::Arc;

struct InterruptedCopy {
    inner: authorization::Provider,
    fail: std::sync::atomic::AtomicBool,
}
impl Home for InterruptedCopy {
    fn authorize<'a>(
        &'a self,
        proof: &'a Proof,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Option<authorization::Answer>>> {
        self.inner.authorize(proof)
    }
    fn user<'a>(
        &'a self,
        name: &'a str,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Option<codeyard_db_core::models::User>>>
    {
        self.inner.user(name)
    }
    fn repository_source<'a>(
        &'a self,
        owner: &'a str,
        name: &'a str,
        proof: &'a Proof,
    ) -> futures_util::future::BoxFuture<
        'a,
        anyhow::Result<Option<codeyard_db_core::cells::repositories::Source>>,
    > {
        self.inner.repository_source(owner, name, proof)
    }
    fn snapshot<'a>(
        &'a self,
        request: &'a codeyard_db_core::cells::repositories::SnapshotRequest,
        output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> futures_util::future::BoxFuture<
        'a,
        anyhow::Result<codeyard_db_core::cells::repositories::Snapshot>,
    > {
        Box::pin(async move {
            let snapshot = self.inner.snapshot(request, output).await?;
            anyhow::ensure!(
                !self.fail.load(std::sync::atomic::Ordering::SeqCst),
                "Copy interrupted after receiving objects"
            );
            Ok(snapshot)
        })
    }
}

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
    let bob_root = tempfile::tempdir().unwrap();
    let home = accounts::listen(
        directory_client(one_key.clone()),
        Arc::new(codeyard_app::cells::Native {
            state: codeyard_app::app::AppState::new(
                one.pool.clone(),
                bob_root.path().to_owned(),
                Arc::new(
                    codeyard_storage::ObjectStore::open(bob_root.path(), None)
                        .await
                        .unwrap(),
                ),
                None,
                codeyard_ci::config::CiConfig::default(),
                None,
            ),
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
    codeyard_db_cluster::node_config::upsert_node_configuration(&one.pool, "public", true)
        .await
        .unwrap();
    let mut bob_http = super::cells_frontend::Server::start(
        crate::support::database::router(&one.pool, bob_root.path()).await,
    )
    .await;
    let zero_home = accounts::listen(
        directory_client(zero_key.clone()),
        Arc::new(codeyard_app::cells::Native {
            state: codeyard_app::app::AppState::new(
                zero.pool.clone(),
                root.path().to_owned(),
                Arc::new(
                    codeyard_storage::ObjectStore::open(root.path(), None)
                        .await
                        .unwrap(),
                ),
                None,
                codeyard_ci::config::CiConfig::default(),
                None,
            ),
        }),
        &zero_key,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let alice_object = Object {
        kind: ObjectKind::User,
        id: alice.id,
    };
    let alice_keys = [Key::new(KeyKind::Name, "alice").unwrap()];
    directory
        .reserve(&zero_key.public_hex(), alice_object, &alice_keys, &nonce)
        .await
        .unwrap();
    directory
        .confirm(&zero_key.public_hex(), alice_object, &alice_keys, &nonce)
        .await
        .unwrap();
    let mut zero_record = cell(Prefix::ZERO, &zero_key, vec![zero_home.address.to_string()]);
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
    let pointer_event: i64 =
        sqlx::query_scalar("SELECT id FROM cell_outbox WHERE topic='pointer' ORDER BY id LIMIT 1")
            .fetch_one(&zero.raw)
            .await
            .unwrap();
    assert!(codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
        .await
        .unwrap());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM remote_pointers WHERE user_id=$1 AND resource_id=$2 AND active"
        )
        .bind(user.id)
        .bind(repository.id)
        .fetch_one(&one.raw)
        .await
        .unwrap(),
        1
    );
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
    let fork_proof = Proof::Token {
        home: Prefix::new(1),
        hash: token_hash(&value),
    };
    let remote_source = one.pool.foreign_authorization().unwrap();
    let source = remote_source
        .repository_source("alice", "private", &fork_proof)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.repository.id, repository.id);
    assert!(source.repository.disk_path.is_empty());
    assert_eq!(source.repository.storage_node_id, None);
    let response = client
        .put(format!(
            "{}/api/v3/repos/alice/private/contents/copy.txt",
            http.url()
        ))
        .bearer_auth(&alice_token)
        .json(&serde_json::json!({"message":"copy source", "content":"Y29weSBieXRlcwo="}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let document: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 201, "{document}");
    let snapshot_request = codeyard_db_core::cells::repositories::SnapshotRequest {
        repository_id: repository.id,
        reader_id: user.id,
        owner: "alice".into(),
        name: "private".into(),
        proof: fork_proof.clone(),
        default_branch_only: false,
        branch: None,
        maximum_bytes: 1024 * 1024,
        timeout_seconds: 30,
    };
    let mut bytes = Vec::new();
    let snapshot = remote_source
        .snapshot(&snapshot_request, &mut bytes)
        .await
        .unwrap();
    snapshot.validate(&snapshot_request).unwrap();
    let pack = root.path().join("test-fork.pack");
    tokio::fs::write(&pack, &bytes).await.unwrap();
    let copy_path = root.path().join("test-fork.git");
    let copy = codeyard_repos_core::repositories::store::RepoStore::from_disk_path(
        copy_path.to_str().unwrap(),
    );
    copy.initialize(None, &snapshot.default_branch)
        .await
        .unwrap();
    copy.index_pack(&pack, true).await.unwrap();
    copy.set_refs(&snapshot.refs).await.unwrap();
    let copied = tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(&copy_path)
        .args(["show", "refs/heads/main:copy.txt"])
        .output()
        .await
        .unwrap();
    assert!(copied.status.success());
    assert_eq!(copied.stdout, b"copy bytes\n");
    let mut forged = snapshot_request.clone();
    forged.repository_id = native_repository.id;
    assert!(remote_source
        .snapshot(&forged, &mut Vec::new())
        .await
        .is_err());
    forged = snapshot_request.clone();
    forged.reader_id = alice.id;
    assert!(remote_source
        .snapshot(&forged, &mut Vec::new())
        .await
        .is_err());
    let serialized = serde_json::to_value(&source).unwrap();
    assert!(serialized.get("git_url").is_none());
    let mut forged_source = source.clone();
    forged_source.repository.disk_path = "/private/native/storage".into();
    assert!(forged_source.validate(Prefix::ZERO).is_err());
    forged_source = source.clone();
    forged_source.repository.owner_user_id = Some(user.id);
    assert!(forged_source.validate(Prefix::ZERO).is_err());
    source.retain(&one.pool).await.unwrap();
    assert!(
        codeyard_db_repos::repositories::records::get_repository_by_id(&one.pool, repository.id)
            .await
            .unwrap()
            .unwrap()
            .is_stub
    );
    let credential = codeyard_db_core::queries::users::accounts::AccountCredential::Token(
        token.id,
        &token.token_hash,
    );
    let authority = codeyard_db_core::queries::credentials::Actor::from((user.id, &credential));
    let admission = codeyard_db_core::cells::repositories::Admission::new(
        source.clone(),
        authority.clone(),
        value.clone(),
    )
    .unwrap();
    let mut peer_bound = admission.clone();
    peer_bound.bind_native_peer(&zero_key.public_hex()).unwrap();
    assert!(serde_json::to_value(&peer_bound)
        .unwrap()
        .get("native_peer")
        .is_none());
    let mut forged_admission = admission.clone();
    forged_admission.actor.credential = Some(
        codeyard_db_core::queries::users::accounts::OwnedAccountCredential::Token(
            token.id + 1,
            token.token_hash.clone(),
        ),
    );
    forged_admission.source.credential_id = Some(token.id + 1);
    assert!(forged_admission.check(&one.pool).await.is_err());
    assert_eq!(source.credential_id, Some(token.id));
    let operations = [
        codeyard_repos_core::wire::Operation::CompareForeignPullHead {
            admission: Box::new(admission.clone()),
            base: "a".repeat(40),
            branch: "topic".into(),
        },
        codeyard_repos_core::wire::Operation::FetchForeignPullHead {
            admission: Box::new(admission.clone()),
            number: 1,
            branch: "topic".into(),
            head: "a".repeat(40),
            expected_head: None,
        },
    ];
    for operation in &operations {
        for (role, allowed) in [
            ("web", true),
            ("ssh", true),
            ("storage", false),
            ("runner", false),
        ] {
            assert_eq!(
                codeyard_repos_rpc::repository::authorize_operation(&[role.to_owned()], operation)
                    .is_ok(),
                allowed
            );
        }
    }
    let operation = codeyard_repos_core::wire::Operation::InitializeForeignCopy {
        admission: Box::new(admission.clone()),
        default_branch: "main".into(),
        default_branch_only: false,
    };
    for (role, allowed) in [
        ("web", true),
        ("ssh", true),
        ("storage", false),
        ("runner", false),
    ] {
        assert_eq!(
            codeyard_repos_rpc::repository::authorize_operation(&[role.to_owned()], &operation)
                .is_ok(),
            allowed
        );
    }
    let native_store =
        codeyard_repos_core::repositories::store::repo_store(&native_repository, None);
    assert!(native_store
        .initialize_foreign_copy(
            &one.pool,
            native_repository.id,
            &admission,
            &codeyard_repos_core::repositories::store::CopyOptions {
                default_branch: "main".into(),
                template: false,
                default_branch_only: false
            }
        )
        .await
        .is_err());
    assert!(native_store.refs().await.unwrap().is_empty());
    let interrupted = Arc::new(InterruptedCopy {
        inner: remote_source.clone(),
        fail: true.into(),
    });
    one.pool.set_foreign_authorization(interrupted.clone());
    let notifications = codeyard_notifications::broadcast::NotificationBroadcaster::new(&one.pool);
    let fork_options = || codeyard_repos_core::repositories::lifecycle::ForkOptions {
        owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(
            user.id,
        ),
        name: Some("copied-fork"),
        default_branch_only: false,
        properties: &[],
        authority: Some(&authority),
        foreign_source: Some(&admission),
    };
    assert!(codeyard_repos_core::repositories::lifecycle::fork_into(
        &one.pool,
        None,
        bob_root.path(),
        &notifications,
        fork_options(),
        repository.id,
        "alice"
    )
    .await
    .is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM repositories WHERE name='copied-fork'")
            .fetch_one(&one.raw)
            .await
            .unwrap(),
        0
    );
    interrupted
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let forked = codeyard_repos_core::repositories::lifecycle::fork_into(
        &one.pool,
        None,
        bob_root.path(),
        &notifications,
        fork_options(),
        repository.id,
        "alice",
    )
    .await
    .unwrap();
    assert_eq!(forked.forked_from_id, Some(repository.id));
    assert_eq!(forked.home_cell, 1);
    assert!(forked.is_private && forked.is_fork && !forked.is_stub);
    let clone_path = bob_root.path().join("copied-clone");
    let cloned = tokio::process::Command::new("git")
        .arg("clone")
        .arg(&forked.disk_path)
        .arg(&clone_path)
        .output()
        .await
        .unwrap();
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    assert_eq!(
        tokio::fs::read(clone_path.join("copy.txt")).await.unwrap(),
        b"copy bytes\n"
    );
    codeyard_repos_core::repositories::lifecycle::delete(
        &one.pool,
        None,
        authority.clone(),
        forked.id,
    )
    .await
    .unwrap();
    one.pool.set_foreign_authorization(remote_source.clone());
    let created = client
        .post(format!("{}/api/v3/repos/alice/private/forks", http.url()))
        .bearer_auth(&value)
        .json(&serde_json::json!({"name":"http-fork"}))
        .send()
        .await
        .unwrap();
    let status = created.status();
    let created: serde_json::Value = created.json().await.unwrap();
    assert_eq!(status, 202, "{created}");
    assert_eq!(created["full_name"], "bob/http-fork");
    assert_eq!(created["parent"]["id"], repository.id);
    assert_eq!(created["source"]["full_name"], "alice/private");
    let fork_id = created["id"].as_i64().unwrap();
    let stored = codeyard_db_repos::repositories::records::get_repository_by_id(&one.pool, fork_id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.is_fork && stored.is_private && !stored.is_stub && stored.home_cell == 1);
    assert!(
        codeyard_db_repos::repositories::records::get_repository_by_id(&zero.pool, fork_id)
            .await
            .unwrap()
            .is_none()
    );
    let discovered = zero
        .pool
        .foreign_authorization()
        .unwrap()
        .repository_fork("bob", repository.id, &fork_proof)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(discovered.repository.id, fork_id);
    assert_eq!(discovered.repository.name, "http-fork");
    assert_eq!(discovered.repository.forked_from_id, Some(repository.id));
    assert_eq!(discovered.reader_id, user.id);
    discovered.retain(&zero.pool).await.unwrap();
    assert_eq!(
        codeyard_db_repos::repositories::records::get_repository_by_id(&zero.pool, fork_id)
            .await
            .unwrap()
            .unwrap()
            .forked_from_id,
        Some(repository.id)
    );
    assert!(zero
        .pool
        .foreign_authorization()
        .unwrap()
        .repository_fork("bob", native_repository.id, &fork_proof)
        .await
        .unwrap()
        .is_none());
    let branch = client.post(format!("{}/api/v3/repos/bob/http-fork/git/refs", bob_http.url()))
        .bearer_auth(&value).json(&serde_json::json!({"ref":"refs/heads/foreign-topic", "sha":snapshot.refs["refs/heads/main"]}))
        .send().await.unwrap();
    let status = branch.status();
    let body = branch.text().await.unwrap();
    assert_eq!(status, 201, "{body}");
    let pushed = client.put(format!("{}/api/v3/repos/bob/http-fork/contents/foreign.txt", bob_http.url()))
        .bearer_auth(&value).json(&serde_json::json!({"branch":"foreign-topic", "message":"Foreign head", "content":"Zm9yZWlnbiBwdWxsCg=="}))
        .send().await.unwrap();
    let status = pushed.status();
    let pushed: serde_json::Value = pushed.json().await.unwrap();
    assert_eq!(status, 201, "{pushed}");
    let foreign_tip = pushed["commit"]["sha"].as_str().unwrap();
    let created_pull = client.post(format!("{}/api/v3/repos/alice/private/pulls", http.url()))
        .bearer_auth(&value).json(&serde_json::json!({"title":"Foreign fork pull", "head":"bob:foreign-topic", "base":"main", "maintainer_can_modify":true}))
        .send().await.unwrap();
    let status = created_pull.status();
    let created_pull: serde_json::Value = created_pull.json().await.unwrap();
    assert_eq!(status, 201, "{created_pull}");
    assert_eq!(created_pull["head"]["repo"]["id"], fork_id);
    assert_eq!(created_pull["head"]["sha"], foreign_tip);
    let number = created_pull["number"].as_i64().unwrap() as i32;
    assert_eq!(
        codeyard_repos_core::repositories::store::repo_store(&repository, None)
            .commit(&format!("refs/pull/{number}/head"))
            .await
            .unwrap()
            .sha,
        foreign_tip
    );
    let head_view = client.post(format!("{}/api/graphql", http.url())).bearer_auth(&value)
        .json(&serde_json::json!({"query":format!(r#"{{repository(owner:"alice",name:"private"){{pullRequest(number:{number}){{headRepository{{id nameWithOwner}} headRepositoryOwner{{login}} headRefOid}}}}}}"#) }))
        .send().await.unwrap().json::<serde_json::Value>().await.unwrap();
    assert!(head_view.get("errors").is_none(), "{head_view}");
    assert_eq!(
        head_view["data"]["repository"]["pullRequest"]["headRepository"]["nameWithOwner"],
        "bob/http-fork"
    );
    assert_eq!(
        head_view["data"]["repository"]["pullRequest"]["headRefOid"],
        foreign_tip
    );
    let gql_branch = client
        .post(format!(
            "{}/api/v3/repos/bob/http-fork/git/refs",
            bob_http.url()
        ))
        .bearer_auth(&value)
        .json(&serde_json::json!({"ref":"refs/heads/graphql-topic", "sha":foreign_tip}))
        .send()
        .await
        .unwrap();
    assert_eq!(gql_branch.status(), 201);
    let graphql_pull = client.post(format!("{}/api/graphql", http.url())).bearer_auth(&value)
        .json(&serde_json::json!({
            "query":r#"mutation($base:ID!,$head:ID!){createPullRequest(input:{repositoryId:$base,headRepositoryId:$head,headRefName:"graphql-topic",baseRefName:"main",title:"Foreign GraphQL pull"}){pullRequest{number headRefOid}}}"#,
            "variables":{"base":codeyard_model::node_id::encode("R",repository.id),"head":codeyard_model::node_id::encode("R",fork_id)}
        })).send().await.unwrap().json::<serde_json::Value>().await.unwrap();
    assert!(graphql_pull.get("errors").is_none(), "{graphql_pull}");
    assert_eq!(
        graphql_pull["data"]["createPullRequest"]["pullRequest"]["headRefOid"],
        foreign_tip
    );
    assert!(
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
            &one.pool,
            created_pull["id"].as_i64().unwrap()
        )
        .await
        .unwrap()
        .is_none()
    );
    // A push to the fork in cell one moves the pull request in cell zero.
    let moved = client.put(format!("{}/api/v3/repos/bob/http-fork/contents/followed.txt", bob_http.url()))
        .bearer_auth(&value).json(&serde_json::json!({"branch":"foreign-topic", "message":"Followed head", "content":"Zm9sbG93ZWQK"}))
        .send().await.unwrap();
    let status = moved.status();
    let moved: serde_json::Value = moved.json().await.unwrap();
    assert_eq!(status, 201, "{moved}");
    let followed_tip = moved["commit"]["sha"].as_str().unwrap().to_owned();
    let push_event: i64 = sqlx::query_scalar("SELECT id FROM cell_outbox WHERE topic='repository.push' AND root_id=$1 AND delivered_at IS NULL ORDER BY id DESC LIMIT 1")
        .bind(fork_id).fetch_one(&one.raw).await.unwrap();
    let refreshes = codeyard_app::cells::Refreshes {
        state: codeyard_app::app::AppState::new(
            zero.pool.clone(),
            root.path().to_owned(),
            Arc::new(
                codeyard_storage::ObjectStore::open(root.path(), None)
                    .await
                    .unwrap(),
            ),
            None,
            codeyard_ci::config::CiConfig::default(),
            None,
        ),
    };
    let drain = |pool: &codeyard_db_core::DbPool| {
        let pool = pool.clone();
        let refreshes = &refreshes;
        async move {
            for _ in 0..50 {
                if !codeyard_db_core::cells::outbox::deliver_with(&pool, refreshes)
                    .await
                    .unwrap()
                {
                    return;
                }
            }
            panic!("cell outbox did not drain");
        }
    };
    drain(&one.pool).await;
    drain(&zero.pool).await;
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM cell_outbox WHERE delivered_at IS NULL AND topic IN ('repository.push','pull.refresh')")
        .fetch_one(&one.raw).await.unwrap() + sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM cell_outbox WHERE delivered_at IS NULL AND topic IN ('repository.push','pull.refresh')")
        .fetch_one(&zero.raw).await.unwrap(), 0);
    let followed = |number: i32| {
        let repository = repository.clone();
        async move {
            codeyard_repos_core::repositories::store::repo_store(&repository, None)
                .commit(&format!("refs/pull/{number}/head"))
                .await
                .unwrap()
                .sha
        }
    };
    assert_eq!(followed(number).await, followed_tip);
    let stored_pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
        &zero.pool,
        created_pull["id"].as_i64().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stored_pull.head_sha, followed_tip);
    let listed = client
        .get(format!(
            "{}/api/v3/repos/alice/private/pulls/{number}",
            http.url()
        ))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(listed["head"]["sha"], followed_tip.as_str());
    // The same event delivered twice queues no second refresh.
    let pushed_event: serde_json::Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT payload FROM cell_outbox WHERE id=$1")
            .bind(push_event)
            .fetch_one(&one.raw)
            .await
            .unwrap(),
    )
    .unwrap();
    one.pool
        .foreign_authorization()
        .unwrap()
        .send_push(&codeyard_db_core::cells::pulls::Event {
            id: push_event,
            change: serde_json::from_value(pushed_event).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM cell_outbox WHERE delivered_at IS NULL AND topic='pull.refresh'"
        )
        .fetch_one(&zero.raw)
        .await
        .unwrap(),
        0
    );
    // The head cell streams the branch only to the recorded publisher of a live binding.
    let binding = codeyard_db_core::cells::pulls::Binding {
        base_repo_id: repository.id,
        number,
        head_repo_id: fork_id,
        head_branch: "foreign-topic".into(),
    };
    for (binding, publisher_id) in [
        (binding.clone(), alice.id),
        (
            codeyard_db_core::cells::pulls::Binding {
                number: number + 1000,
                ..binding.clone()
            },
            user.id,
        ),
        (
            codeyard_db_core::cells::pulls::Binding {
                head_branch: "main".into(),
                ..binding.clone()
            },
            user.id,
        ),
    ] {
        let request = codeyard_db_core::cells::pulls::HeadRequest {
            binding,
            publisher_id,
            maximum_bytes: 1 << 26,
            timeout_seconds: 30,
        };
        let mut sink = Vec::new();
        let refused = zero
            .pool
            .foreign_authorization()
            .unwrap()
            .pull_snapshot(&request, &mut sink)
            .await;
        assert!(!matches!(refused, Ok(Some(_))), "{request:?}");
        assert!(sink.is_empty(), "{request:?}");
    }
    // A push that the base already holds leaves the pull request where it is.
    let unchanged = codeyard_db_core::cells::outbox::deliver_with(&zero.pool, &refreshes)
        .await
        .unwrap();
    assert!(!unchanged);
    assert_eq!(followed(number).await, followed_tip);
    let source_by_id = zero
        .pool
        .foreign_authorization()
        .unwrap()
        .repository_id(fork_id, &fork_proof)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source_by_id.repository.id, fork_id);
    assert!(zero
        .pool
        .foreign_authorization()
        .unwrap()
        .repository_id(repository.id, &fork_proof)
        .await
        .unwrap()
        .is_some());
    let parent = client.post(format!("{}/api/graphql", bob_http.url())).bearer_auth(&value)
        .json(&serde_json::json!({"query":r#"{repository(owner:"bob",name:"http-fork"){parent{id nameWithOwner owner{login} defaultBranchRef{name}}}}"#}))
        .send().await.unwrap().json::<serde_json::Value>().await.unwrap();
    assert!(parent.get("errors").is_none(), "{parent}");
    assert_eq!(
        parent["data"]["repository"]["parent"]["nameWithOwner"],
        "alice/private"
    );
    assert_eq!(
        parent["data"]["repository"]["parent"]["owner"]["login"],
        "alice"
    );
    assert_eq!(
        parent["data"]["repository"]["parent"]["defaultBranchRef"]["name"],
        "main"
    );
    codeyard_repos_core::repositories::lifecycle::delete(
        &one.pool,
        None,
        authority.clone(),
        fork_id,
    )
    .await
    .unwrap();
    let form = client
        .get(format!("{}/alice/private/_forms/fork", http.url()))
        .header("cookie", format!("session_id={session}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = crate::support::http_client::hidden_value(&form, "csrf_token");
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf_token", &csrf)
        .append_pair("repository_id", &repository.id.to_string())
        .finish();
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = no_redirect
        .post(format!("{}/alice/private/_forms/fork", http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let document = response.text().await.unwrap();
    assert_eq!(status, 303, "{document}");
    assert_eq!(location, "/bob/private");
    let native_fork =
        codeyard_db_repos::repositories::records::get_repository(&one.pool, "bob", "private")
            .await
            .unwrap()
            .unwrap();
    codeyard_repos_core::repositories::lifecycle::delete(
        &one.pool,
        None,
        authority.clone(),
        native_fork.id,
    )
    .await
    .unwrap();
    assert!(remote_source
        .repository_source(
            "alice",
            "private",
            &Proof::Token {
                home: Prefix::new(1),
                hash: token_hash("revoked-or-forged"),
            }
        )
        .await
        .unwrap()
        .is_none());
    assert!(
        accounts::Remote::new(router_key.clone(), directory_client(router_key.clone()))
            .unwrap()
            .repository_source("alice", "private", &fork_proof)
            .await
            .is_err()
    );
    // Stars commit in the resource cell and discover that cell through a distinct home pointer.
    let response = client
        .put(format!("{}/api/v3/user/starred/alice/private", http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let star_event: i64 = sqlx::query_scalar("SELECT id FROM cell_outbox WHERE topic='pointer' AND payload::jsonb->>'kind'='star' ORDER BY id DESC LIMIT 1")
        .fetch_one(&zero.raw).await.unwrap();
    assert!(codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
        .await
        .unwrap());
    let star_replay = codeyard_db_core::cells::pointers::Event {
        id: star_event,
        change: codeyard_db_core::cells::pointers::Change {
            kind: codeyard_db_core::cells::pointers::Kind::Star,
            user_id: user.id,
            object: Object {
                kind: ObjectKind::Repository,
                id: repository.id,
            },
            active: true,
        },
    };
    let sender =
        accounts::Remote::new(zero_key.clone(), directory_client(zero_key.clone())).unwrap();
    sender.send_pointer(&star_replay).await.unwrap();
    sender.send_pointer(&star_replay).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM remote_pointers WHERE user_id=$1 AND resource_id=$2 AND kind='star' AND active")
        .bind(user.id).bind(repository.id).fetch_one(&one.raw).await.unwrap(),1);
    for path in ["/api/v3/user/starred", "/api/v3/users/bob/starred"] {
        let response = client
            .get(format!("{}{path}", bob_http.url()))
            .bearer_auth(&value)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let rows: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{rows}");
        assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
        assert_eq!(rows[0]["id"], repository.id, "{rows}");
    }
    let anonymous: serde_json::Value = client
        .get(format!("{}/api/v3/users/bob/starred", bob_http.url()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(anonymous, serde_json::json!([]));
    let response = client
        .get(format!("{}/stars", bob_http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("accept", "application/vnd.codeyard.page+json")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let browser: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{browser}");
    assert_eq!(
        browser["page"]["model"]["Stars"]["repositories"][0]["id"], repository.id,
        "{browser}"
    );
    for path in [
        "/api/users/bob/starred?per_page=101",
        "/api/users/bob/starred?page=0",
        "/api/users/bob/starred?q=%00",
        "/api/users/bad%20name/starred",
    ] {
        assert_eq!(
            client
                .get(format!("{}{path}", http.url()))
                .header("cookie", format!("session_id={session}"))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    // A merged third page scans more than one card batch in the native home as well.
    let mut bulk_ids = Vec::new();
    for number in 0..101 {
        let repo = codeyard_db_repos::repositories::records::create_repository(
            &one.pool,
            user.id,
            &format!("star-page-{number:03}"),
            None,
            false,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        assert!(codeyard_db_repos::repositories::engagement::star_repo(
            &one.pool, user.id, repo.id
        )
        .await
        .unwrap());
        bulk_ids.push(repo.id);
    }
    let response = client
        .get(format!("{}/stars?page=3", bob_http.url()))
        .header("cookie", format!("session_id={session}"))
        .header("accept", "application/vnd.codeyard.page+json")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let browser: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{browser}");
    assert_eq!(
        browser["page"]["model"]["Stars"]["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "{browser}"
    );
    assert_eq!(
        browser["page"]["model"]["Stars"]["has_older"], false,
        "{browser}"
    );
    sqlx::query("DELETE FROM repositories WHERE id=ANY($1)")
        .bind(&bulk_ids)
        .execute(&one.raw)
        .await
        .unwrap();
    let response = client
        .put(format!("{}/api/v3/user/starred/bob/native", bob_http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let mut ids = std::collections::HashSet::new();
    for page in 1..=2 {
        let response = client
            .get(format!(
                "{}/api/v3/user/starred?per_page=1&page={page}",
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
        assert!(
            ids.insert(rows[0]["id"].as_i64().unwrap()),
            "duplicate star across pages: {rows}"
        );
    }
    assert_eq!(
        ids,
        std::collections::HashSet::from([repository.id, native_repository.id])
    );
    let response = client
        .delete(format!("{}/api/v3/user/starred/bob/native", bob_http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let response = client
        .delete(format!("{}/api/v3/user/starred/alice/private", http.url()))
        .bearer_auth(&value)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert!(codeyard_db_core::cells::outbox::deliver_one(&zero.pool)
        .await
        .unwrap());
    sender.send_pointer(&star_replay).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM remote_pointers WHERE user_id=$1 AND resource_id=$2 AND kind='star' AND active")
        .bind(user.id).bind(repository.id).fetch_one(&one.raw).await.unwrap(),0);
    let mut replay = codeyard_db_core::cells::pointers::Event {
        id: pointer_event,
        change: codeyard_db_core::cells::pointers::Change {
            kind: codeyard_db_core::cells::pointers::Kind::Membership,
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
            .send_pointer(&replay)
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
    assert!(remote_source
        .repository_source("alice", "private", &fork_proof)
        .await
        .unwrap()
        .is_none());
    assert!(remote_source
        .snapshot(&snapshot_request, &mut Vec::new())
        .await
        .is_err());
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
        .send_pointer(&replay)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM remote_pointers WHERE user_id=$1 AND resource_id=$2 AND active"
        )
        .bind(user.id)
        .bind(repository.id)
        .fetch_one(&one.raw)
        .await
        .unwrap(),
        0
    );
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
        .send_pointer(&replay)
        .await
        .is_err());
    bob_http.stop().await;
    zero_home.stop().await;
    home.stop().await;
    directory_listener.stop().await;
    directory.close().await;
    one.finish().await;
    zero.finish().await;
    registry.finish().await;
}
