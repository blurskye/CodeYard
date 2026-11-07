//! Copying a whole application database between SQLite and PostgreSQL.

use crate::support::native_database as native;

use codeyard_db_core::pool::{PgPool, SqlitePool};
use native::{NativeDatabase, PostgresDatabase};
use uuid::Uuid;

/// Application tables, read from the catalog so a table added later is covered too.
async fn sqlite_tables(source: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_gitserv_*' ORDER BY name",
    )
    .fetch_all(source)
    .await
    .unwrap()
}

async fn sqlite_count(source: &SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM \"{table}\""))
        .fetch_one(source)
        .await
        .unwrap()
}

async fn postgres_count(target: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM \"{table}\""))
        .fetch_one(target)
        .await
        .unwrap()
}

/// These tests share one PostgreSQL database, and each copy changes its tables with
/// DDL and takes table locks. Overlapping copies deadlocked once, so they run one at a time.
static TURNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Seed {
    alice: i64,
    bob: i64,
    repo: i64,
}

/// Application data through the db API, plus values the API does not set: a 1 MiB
/// blob, an admin flag, an avatar file id, an INET address, JSON arrays and non-ASCII
/// text, and a 64-bit integer past 2^53.
async fn populate(source: &NativeDatabase) -> Seed {
    let pool = &source.pool;
    let alice = codeyard_db_identity::users::accounts::create_user(
        pool,
        "alice",
        Some("alice@example.test"),
    )
    .await
    .unwrap();
    let bob = codeyard_db_identity::users::accounts::create_user(pool, "bob", None)
        .await
        .unwrap();
    let repo = codeyard_db_repos::repositories::records::create_repository(
        pool,
        alice.id,
        "demo",
        Some("Demo"),
        false,
        "alice/demo.git",
    )
    .await
    .unwrap();
    codeyard_db_repos::milestones::create_milestone(
        pool,
        alice.id,
        repo.id,
        "v1",
        None,
        Some("2026-10-20"),
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::discovery::create_forked_repository(
        pool,
        codeyard_db_repos::repositories::discovery::NewForkedRepository {
            owner_id: bob.id,
            name: "demo-fork",
            description: None,
            is_private: false,
            disk_path: "bob/demo-fork.git",
            forked_from: repo.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::engagement::star_repo(pool, bob.id, repo.id)
        .await
        .unwrap();
    codeyard_db_repos::repositories::engagement::watch_repo(pool, bob.id, repo.id)
        .await
        .unwrap();
    codeyard_db_repos::issues::create_issue(
        pool,
        repo.id,
        alice.id,
        "Naïve café 名前",
        "Emoji 🦀 and a\nsecond line",
    )
    .await
    .unwrap();
    let session = native::token();
    codeyard_db_identity::users::sessions::create_session(
        pool,
        &session,
        alice.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        pool,
        alice.id,
        &session,
        "cli",
        &"f".repeat(64),
        "read",
        None,
    )
    .await
    .unwrap();

    // Tenant-scoped typed metadata must survive both copy directions unchanged.
    let org = match codeyard_db_orgs::organizations::profiles::create_organization(
        pool,
        alice.id,
        &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
            "metadata-owner",
            codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                .unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) => org,
        other => panic!("organization fixture: {other:?}"),
    };
    let settings=codeyard_db_orgs::organizations::settings::Changes::parse(&serde_json::json!({"company":"Metadata café 名前","email":"public@example.test","billing_email":"private@example.test","location":"Montréal","blog":"https://example.test/metadata","default_repository_permission":"write","members_can_create_private_repositories":true,"members_can_fork_private_repositories":false,"has_organization_projects":false,"web_commit_signoff_required":true,"deploy_keys_enabled_for_repositories":false})).unwrap();
    assert!(matches!(
        codeyard_db_orgs::organizations::settings::change(pool, alice.id, org.id, &settings)
            .await
            .unwrap(),
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(_)
    ));
    let metadata_repo = codeyard_db_repos::repositories::creation::insert_repository(
        pool,
        codeyard_db_repos::repositories::creation::NewRepository {
            owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
                actor_id: alice.id,
                target: codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(
                    org.id,
                ),
            },
            name: "metadata",
            description: None,
            is_private: true,
            disk_path: "metadata/repository.git",
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let definition=serde_json::from_value(serde_json::json!({"property_name":"traits","value_type":"multi_select","required":true,"default_value":["ci","café"],"allowed_values":["ci","café","名前"],"description":"Unicode metadata ✓","values_editable_by":"org_and_repo_actors"})).unwrap();
    assert!(matches!(
        codeyard_db_orgs::organizations::properties::define(pool, alice.id, org.id, &[definition])
            .await
            .unwrap(),
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(_)
    ));
    assert!(matches!(
        codeyard_db_orgs::organizations::properties::set_values(
            pool,
            alice.id,
            org.id,
            &["metadata".into()],
            &[
                codeyard_db_core::queries::organizations::properties::ValueInput {
                    property_name: "traits".into(),
                    value: Some(
                        codeyard_db_core::queries::organizations::properties::PropertyValue::Strings(
                            vec!["名前".into()]
                        )
                    )
                }
            ]
        )
        .await
        .unwrap(),
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(())
    ));
    assert!(metadata_repo.id > repo.id);

    let raw = &source.raw;
    let file_id = *Uuid::new_v4().as_bytes();
    sqlx::query(
        "INSERT INTO files (id, user_id, filename, original_name, content_type, file_size, sha256_hex, object_key, reason, is_active, created_at, updated_at, visibility) \
         VALUES (?1, ?2, 'avatar.png', 'avatar.png', 'image/png', 3, ?3, 'objects/avatar', 'avatar', 1, '2026-10-11T10:00:00.000001Z', '2026-10-11T10:00:00.000001Z', 'owner')",
    )
    .bind(&file_id[..])
    .bind(alice.id)
    .bind("ab".repeat(32))
    .execute(raw)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET is_admin = 1, avatar_file_id = ?1 WHERE id = ?2")
        .bind(&file_id[..])
        .bind(alice.id)
        .execute(raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO two_factor_auth (user_id, totp_secret_encrypted, totp_nonce) VALUES (?1, randomblob(1048576), randomblob(12))")
        .bind(alice.id)
        .execute(raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (id, user_id, user_agent, ip_address, expires_at, created_at) VALUES ('session-1', ?1, 'curl/8.5', '2001:db8::1/128', '2030-01-01T00:00:00.000000Z', '2026-10-11T10:00:00.123456Z')")
        .bind(alice.id)
        .execute(raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO file_reason_limits (reason, max_file_size_bytes, max_uploads_per_hour, max_bytes_per_hour, max_uploads_per_day, max_bytes_per_day, allowed_content_types, requires_auth, description) VALUES ('video_test', 9007199254740993, 1, 1, 1, 9007199254740993, '[\"video/mp4\",\"video/webm\"]', 0, 'Vidéo ✓ 名前')")
        .execute(raw)
        .await
        .unwrap();

    // Copy nonempty shared provenance, inventory and a durable pending job, not just their schemas.
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::json;
    let org: i64 = sqlx::query_scalar(
        "INSERT INTO organizations(name) VALUES('copied-provenance') RETURNING id",
    )
    .fetch_one(raw)
    .await
    .unwrap();
    sqlx::query("INSERT INTO org_memberships(org_id,user_id,role) VALUES(?1,?2,'owner')")
        .bind(org)
        .bind(alice.id)
        .execute(raw)
        .await
        .unwrap();
    let provenance_repo: i64 = sqlx::query_scalar("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path) VALUES('org',?1,'inventory','copied-provenance/inventory.git') RETURNING id").bind(org).fetch_one(raw).await.unwrap();
    let hash = "d".repeat(64);
    let digest = format!("sha256:{hash}");
    let statement = json!({"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"Copied 🦀","digest":{"sha256":hash}}],"predicateType":"https://slsa.dev/provenance/v1","predicate":{}});
    let bundle = codeyard_db_security::attestation_validation::PreparedBundle::new(&json!({"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","verificationMaterial":{"publicKey":{"hint":"copy fixture"}},"dsseEnvelope":{"payloadType":"application/vnd.in-toto+json","payload":STANDARD.encode(statement.to_string()),"signatures":[{"sig":STANDARD.encode("unverified copy fixture")}]}})).unwrap();
    for id in [repo.id, provenance_repo] {
        codeyard_db_security::attestation_validation::create(pool, alice.id, id, &bundle)
            .await
            .unwrap();
    }
    let write = codeyard_db_identity::users::tokens::create_token(
        pool,
        alice.id,
        &session,
        "inventory-copy",
        &"e".repeat(64),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let actor = codeyard_db_repos::artifact_metadata::Actor {
        user: alice.id,
        token: write.id,
        repo: None,
    };
    let input = serde_json::from_value(json!({"name":"Copied artifact 🦀","digest":digest,"version":"1.0+копия","registry_url":"https://registry.example.test/","path":"copied/名前"})).unwrap();
    codeyard_db_repos::artifact_metadata::storage(pool, actor, org, &input)
        .await
        .unwrap();
    let input = serde_json::from_value(json!({"logical_environment":"production","deployments":[{"name":"Copied artifact 🦀","digest":digest,"deployment_name":"copied-pod"}]})).unwrap();
    codeyard_db_repos::artifact_metadata::enqueue(pool, actor, org, "copied-cluster", &input)
        .await
        .unwrap();

    Seed {
        alice: alice.id,
        bob: bob.id,
        repo: repo.id,
    }
}

#[tokio::test]
async fn copies_repository_with_a_deleted_creator_to_postgres() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    let creator =
        codeyard_db_identity::users::accounts::create_user(&source.pool, "former-creator", None)
            .await
            .unwrap();
    let org: i64 =
        sqlx::query_scalar("INSERT INTO organizations(name) VALUES ('organization') RETURNING id")
            .fetch_one(&source.raw)
            .await
            .unwrap();
    sqlx::query("INSERT INTO repositories(owner_kind, owner_org_id, name, disk_path, created_by_id) VALUES ('org', ?1, 'kept', 'kept.git', ?2)")
        .bind(org).bind(creator.id).execute(&source.raw).await.unwrap();
    sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(creator.id)
        .execute(&source.raw)
        .await
        .unwrap();
    let retained: i64 =
        sqlx::query_scalar("SELECT created_by_id FROM repositories WHERE name = 'kept'")
            .fetch_one(&source.raw)
            .await
            .unwrap();
    assert_eq!(retained, creator.id);
    let target = PostgresDatabase::new().await;
    codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .unwrap();
    let copied: i64 =
        sqlx::query_scalar("SELECT created_by_id FROM repositories WHERE name = 'kept'")
            .fetch_one(&target.raw)
            .await
            .unwrap();
    assert_eq!(copied, creator.id);
}

#[tokio::test]
async fn copies_every_table_and_value_to_postgres() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    let seed = populate(&source).await;
    let target = PostgresDatabase::new().await;

    let counts = codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .expect("copy SQLite to PostgreSQL");

    let tables = sqlite_tables(&source.raw).await;
    assert_eq!(
        counts
            .iter()
            .map(|count| count.table.clone())
            .collect::<Vec<_>>(),
        tables
    );
    for table in &tables {
        let expected = sqlite_count(&source.raw, table).await;
        assert_eq!(
            postgres_count(&target.raw, table).await,
            expected,
            "rows in {table}"
        );
        let reported = counts.iter().find(|count| &count.table == table).unwrap();
        assert_eq!(reported.rows, expected, "reported rows in {table}");
    }

    let target_pool = &target.raw;
    let admin: bool = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1")
        .bind(seed.alice)
        .fetch_one(target_pool)
        .await
        .unwrap();
    assert!(admin);

    let avatar: Vec<u8> = sqlx::query_scalar("SELECT avatar_file_id FROM users WHERE id = ?1")
        .bind(seed.alice)
        .fetch_one(&source.raw)
        .await
        .unwrap();
    let copied_avatar: String =
        sqlx::query_scalar("SELECT avatar_file_id::text FROM users WHERE id = $1")
            .bind(seed.alice)
            .fetch_one(target_pool)
            .await
            .unwrap();
    assert_eq!(
        copied_avatar,
        Uuid::from_slice(&avatar).unwrap().to_string()
    );

    let secret: Vec<u8> =
        sqlx::query_scalar("SELECT totp_secret_encrypted FROM two_factor_auth WHERE user_id = ?1")
            .bind(seed.alice)
            .fetch_one(&source.raw)
            .await
            .unwrap();
    let copied_secret: Vec<u8> =
        sqlx::query_scalar("SELECT totp_secret_encrypted FROM two_factor_auth WHERE user_id = $1")
            .bind(seed.alice)
            .fetch_one(target_pool)
            .await
            .unwrap();
    assert_eq!(secret.len(), 1 << 20);
    assert!(copied_secret == secret, "1 MiB blob changed");

    let (address, created): (String, String) = sqlx::query_as(
        "SELECT host(ip_address) || '/' || masklen(ip_address), to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM sessions WHERE id = 'session-1'",
    )
    .fetch_one(target_pool)
    .await
    .unwrap();
    assert_eq!(address, "2001:db8::1/128");
    assert_eq!(created, "2026-10-11T10:00:00.123456Z");

    // A due date is a calendar day. PostgreSQL stores it as midnight UTC of that day.
    let due: String = sqlx::query_scalar(
        "SELECT to_char(due_date AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI') FROM milestones WHERE title = 'v1'",
    )
    .fetch_one(target_pool)
    .await
    .unwrap();
    assert_eq!(due, "2026-10-20 00:00");

    let title: String = sqlx::query_scalar("SELECT title FROM issues WHERE repo_id = $1")
        .bind(seed.repo)
        .fetch_one(target_pool)
        .await
        .unwrap();
    assert_eq!(title, "Naïve café 名前");

    let limit: (i64, Vec<String>, bool, String) = sqlx::query_as(
        "SELECT max_bytes_per_day, allowed_content_types, requires_auth, description FROM file_reason_limits WHERE reason = 'video_test'",
    )
    .fetch_one(target_pool)
    .await
    .unwrap();
    assert_eq!(
        limit,
        (
            9007199254740993,
            vec!["video/mp4".to_owned(), "video/webm".to_owned()],
            false,
            "Vidéo ✓ 名前".to_owned()
        )
    );

    // Counters and visibility are copied as values; the triggers that maintain them
    // must not run again, or the counts would double.
    let from_source: Vec<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT id, stars_count, watchers_count, forks_count, is_private, visibility_locked FROM repositories ORDER BY id",
    )
    .fetch_all(&source.raw)
    .await
    .unwrap();
    let copied: Vec<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT id, stars_count::bigint, watchers_count::bigint, forks_count::bigint, is_private::integer::bigint, visibility_locked::integer::bigint FROM repositories ORDER BY id",
    )
    .fetch_all(target_pool)
    .await
    .unwrap();
    assert_eq!(copied, from_source);
    let parent = from_source.iter().find(|row| row.0 == seed.repo).unwrap();
    assert_eq!(
        (parent.1, parent.2, parent.3),
        (1, 1, 1),
        "stars, watchers and forks"
    );

    // New rows must not collide with copied ids.
    let carol = codeyard_db_identity::users::accounts::create_user(&target.pool, "carol", None)
        .await
        .unwrap();
    assert!(carol.id > seed.bob, "sequence continues past copied ids");
}

/// Copying out of PostgreSQL again returns every row unchanged.
#[tokio::test]
async fn round_trip_through_postgres_keeps_every_row() {
    let _turn = TURNS.lock().await;
    let original = NativeDatabase::new().await;
    populate(&original).await;
    let target = PostgresDatabase::new().await;
    codeyard_db_cluster::copy::copy_database(&original.url, &target.connection_url())
        .await
        .expect("copy SQLite to PostgreSQL");

    let restored = NativeDatabase::new().await;
    codeyard_db_cluster::copy::copy_database(&target.connection_url(), &restored.url)
        .await
        .expect("copy PostgreSQL to SQLite");

    let tables = sqlite_tables(&original.raw).await;
    let mut connection = restored.raw.acquire().await.unwrap();
    let path = original.url.trim_start_matches("sqlite://");
    sqlx::query("ATTACH DATABASE ?1 AS original")
        .bind(path)
        .execute(&mut *connection)
        .await
        .unwrap();
    for table in &tables {
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
                .bind(table)
                .fetch_all(&mut *connection)
                .await
                .unwrap();
        // Valid JSON text is compared in its minified form, and a timestamp's
        // 9-digit and 6-digit spellings are the same instant.
        let select = columns
            .iter()
            .map(|column| {
                let column = format!("\"{column}\"");
                format!(
                    "CASE WHEN typeof({column}) = 'text' THEN replace(CASE WHEN json_valid({column}) THEN json({column}) ELSE {column} END, '000Z', 'Z') ELSE {column} END"
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        for (left, right) in [("original", "main"), ("main", "original")] {
            let differing: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM (SELECT {select} FROM {left}.\"{table}\" EXCEPT SELECT {select} FROM {right}.\"{table}\")"
            ))
            .fetch_one(&mut *connection)
            .await
            .unwrap();
            assert_eq!(
                differing, 0,
                "rows of {table} differ between {left} and {right}"
            );
        }
    }
    drop(connection);

    let dave = codeyard_db_identity::users::accounts::create_user(&restored.pool, "dave", None)
        .await
        .unwrap();
    let largest: i64 = sqlx::query_scalar("SELECT max(id) FROM users")
        .fetch_one(&original.raw)
        .await
        .unwrap();
    assert!(dave.id > largest, "SQLite id continues past copied rows");
}

#[tokio::test]
async fn non_empty_destination_is_refused_and_left_untouched() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    populate(&source).await;
    let target = PostgresDatabase::new().await;
    codeyard_db_identity::users::accounts::create_user(&target.pool, "existing", None)
        .await
        .unwrap();

    let error = codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .expect_err("a populated destination is refused");
    assert!(
        format!("{error:#}").contains("already has rows"),
        "{error:#}"
    );

    let target_pool = &target.raw;
    assert_eq!(postgres_count(target_pool, "users").await, 1);
    assert_eq!(postgres_count(target_pool, "repositories").await, 0);
    assert_eq!(postgres_count(target_pool, "file_reason_limits").await, 10);
}

#[tokio::test]
async fn missing_baseline_history_is_refused() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    // Removing the latest version leaves a valid prefix, which only the version check refuses.
    sqlx::query("DELETE FROM _gitserv_sqlite_migrations WHERE version = (SELECT max(version) FROM _gitserv_sqlite_migrations)")
        .execute(&source.raw)
        .await
        .unwrap();
    let target = PostgresDatabase::new().await;

    let error = codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .expect_err("a source without baseline history is refused");
    assert!(
        format!("{error:#}").contains("migration versions differ"),
        "{error:#}"
    );
    assert_eq!(postgres_count(&target.raw, "users").await, 0);
}

#[tokio::test]
async fn sqlite_source_checksum_mismatch_is_refused() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    codeyard_db_identity::users::accounts::create_user(&source.pool, "kept", None)
        .await
        .unwrap();
    sqlx::query("UPDATE _gitserv_sqlite_migrations SET sha256 = 'tampered' WHERE version = 1")
        .execute(&source.raw)
        .await
        .unwrap();
    let startup_error = codeyard_db_core::run_migrations(&source.pool)
        .await
        .unwrap_err();
    let target = PostgresDatabase::new().await;

    let error = codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .expect_err("copy must reject the same checksum as startup");
    assert_eq!(error.to_string(), startup_error.to_string());
    assert!(error.to_string().contains("history/checksum mismatch"));
    assert_eq!(postgres_count(&target.raw, "users").await, 0);
    assert_eq!(sqlite_count(&source.raw, "users").await, 1);
    let checksum: String =
        sqlx::query_scalar("SELECT sha256 FROM _gitserv_sqlite_migrations WHERE version = 1")
            .fetch_one(&source.raw)
            .await
            .unwrap();
    assert_eq!(checksum, "tampered");

    let error =
        codeyard_db_cluster::copy::copy_database(&source.url, "postgres://127.0.0.1:1/unused")
            .await
            .expect_err("validate the snapshot before connecting to the destination");
    assert_eq!(error.to_string(), startup_error.to_string());
}

#[tokio::test]
async fn postgres_source_checksum_mismatch_is_refused() {
    let _turn = TURNS.lock().await;
    let source = PostgresDatabase::new().await;
    codeyard_db_identity::users::accounts::create_user(&source.pool, "kept", None)
        .await
        .unwrap();
    sqlx::query("UPDATE _gitserv_migrations SET sha256 = 'tampered' WHERE version = 1")
        .execute(&source.raw)
        .await
        .unwrap();
    let startup_error = codeyard_db_core::run_migrations(&source.pool)
        .await
        .unwrap_err();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("copy.sqlite");
    let url = format!("sqlite://{}", path.display());

    let error = codeyard_db_cluster::copy::copy_database(&source.connection_url(), &url)
        .await
        .expect_err("copy must reject the same checksum as startup");
    assert_eq!(error.to_string(), startup_error.to_string());
    assert!(error.to_string().contains("history/checksum mismatch"));
    assert!(!path.exists(), "destination must not be opened");
    assert_eq!(postgres_count(&source.raw, "users").await, 1);
    let checksum: String =
        sqlx::query_scalar("SELECT sha256 FROM _gitserv_migrations WHERE version = 1")
            .fetch_one(&source.raw)
            .await
            .unwrap();
    assert_eq!(checksum, "tampered");
}

#[tokio::test]
async fn postgres_cluster_placement_is_refused_before_sqlite_is_opened() {
    let _turn = TURNS.lock().await;
    let source = PostgresDatabase::new().await;
    let owner = codeyard_db_identity::users::accounts::create_user(&source.pool, "owner", None)
        .await
        .unwrap();
    let repo = create_repository(&source.pool, owner.id, "placed").await;
    let node = codeyard_db_cluster::cluster::register_own_node(
        &source.pool,
        &"a".repeat(64),
        "store",
        &["storage".to_owned()],
        "test",
    )
    .await
    .unwrap();
    assert!(
        codeyard_db_cluster::cluster::place_repository(&source.pool, repo.id, node.id)
            .await
            .unwrap()
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("copy.sqlite");
    let url = format!("sqlite://{}", path.display());

    let error = codeyard_db_cluster::copy::copy_database(&source.connection_url(), &url)
        .await
        .expect_err("SQLite cannot serve placed repositories");
    let message = error.to_string();
    assert!(message.contains("cluster placement"), "{message}");
    assert!(message.contains("consolidate"), "{message}");
    assert!(message.contains("remove placement"), "{message}");
    assert!(!path.exists(), "destination must not be opened");
    let stored =
        codeyard_db_repos::repositories::records::get_repository_by_id(&source.pool, repo.id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(stored.storage_node_id, Some(node.id));

    sqlx::query("UPDATE repositories SET storage_node_id = NULL WHERE id = $1")
        .bind(repo.id)
        .execute(&source.raw)
        .await
        .unwrap();
    codeyard_db_cluster::copy::copy_database(&source.connection_url(), &url)
        .await
        .expect("unplaced repositories may be copied even with membership rows");
    let restored = codeyard_db_core::init_db(&url).await.unwrap();
    let stored = codeyard_db_repos::repositories::records::get_repository_by_id(&restored, repo.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.storage_node_id, None);
    assert_eq!(stored.disk_path, repo.disk_path);
    restored.close().await;
}

#[tokio::test]
async fn failed_copy_leaves_destination_empty() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    let seed = populate(&source).await;
    // A star for a repository that does not exist. The source was written with foreign
    // keys off, so only the destination's check can find it.
    let mut connection = source.raw.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO stars (user_id, repo_id) VALUES (?1, 999999)")
        .bind(seed.bob)
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);

    let target = PostgresDatabase::new().await;
    let error = codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .expect_err("orphaned star breaks the copy");
    assert!(format!("{error:#}").contains("foreign key"), "{error:#}");

    let target_pool = &target.raw;
    for table in sqlite_tables(&source.raw).await {
        // Only the rows the migrations seed remain.
        let expected = match table.as_str() {
            "file_reason_limits" => 10,
            "issue_attribute_guard"
            | "issue_relation_guard"
            | "vulnerability_settings"
            | "actions_settings" => 1,
            "organization_roles" => 6,
            _ => 0,
        };
        assert_eq!(
            postgres_count(target_pool, &table).await,
            expected,
            "rows in {table}"
        );
    }
}

/// SQLite accepts text with a NUL byte, which the HTTP collaboration validator refuses
/// and PostgreSQL cannot store. The copy refuses it up front, names the row and leaves
/// the stored text as it was.
#[tokio::test]
async fn nul_text_is_refused_before_postgres_is_written() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    populate(&source).await;
    sqlx::query("UPDATE issues SET body = body || char(0)")
        .execute(&source.raw)
        .await
        .unwrap();
    let target = PostgresDatabase::new().await;

    let error = codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .expect_err("NUL text is refused");
    let message = format!("{error:#}");
    assert!(
        message.contains("table issues row") && message.contains("NUL"),
        "{message}"
    );

    let body: String = sqlx::query_scalar("SELECT body FROM issues")
        .fetch_one(&source.raw)
        .await
        .unwrap();
    assert!(body.ends_with('\0'), "stored text was altered");
}

#[tokio::test]
async fn copy_cli_refuses_an_unfinished_account_move_before_opening_sqlite() {
    let _turn = TURNS.lock().await;
    let mut source = PostgresDatabase::new().await;
    let user = codeyard_db_identity::users::accounts::create_user(&source.pool, "moving", None)
        .await
        .unwrap();
    sqlx::query("INSERT INTO account_moves(account_kind,account_id,target_cell,state,expires_at) VALUES('user',$1,1,'frozen',clock_timestamp()+INTERVAL '1 hour')")
        .bind(user.id).execute(&source.raw).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("copy.sqlite");
    let target = format!("sqlite://{}", path.display());
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"));
    command
        .args(["db", "copy"])
        .env("COPY_FROM_DATABASE_URL", source.connection_url())
        .env("COPY_TO_DATABASE_URL", &target)
        .env_remove("DATABASE_URL")
        .kill_on_drop(true);
    let refused = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("account move"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !path.exists(),
        "an in-flight move must not become writable through SQLite"
    );
    assert!(codeyard_db_cluster::moves::cancel(
        &source.raw,
        codeyard_db_cluster::moves::Account::new(codeyard_model::cells::ObjectKind::User, user.id)
            .unwrap()
    )
    .await
    .unwrap());
    let copied = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        copied.status.success(),
        "{}",
        String::from_utf8_lossy(&copied.stderr)
    );
    assert!(path.exists());
    source.finish().await;
}

#[tokio::test]
async fn cli_reads_urls_from_environment_and_refuses_passwords_in_arguments() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    let tables = sqlite_tables(&source.raw).await;
    let target = PostgresDatabase::new().await;

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
        .args(["db", "copy", "--from", &source.url])
        .env("COPY_TO_DATABASE_URL", target.connection_url())
        .env_remove("DATABASE_URL")
        .output()
        .expect("run codeyard db copy");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("Copied {} tables", tables.len())),
        "{stdout}"
    );

    // Refused before any connection is attempted, wherever the password is written.
    for url in [
        "postgres://copy:secret@127.0.0.1:9/none",
        "postgres://copy@127.0.0.1:9/none?password=secret",
    ] {
        let refused = std::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .args(["db", "copy", "--from", url, "--to", &source.url])
            .env_remove("DATABASE_URL")
            .output()
            .expect("run codeyard db copy");
        assert!(!refused.status.success(), "{url}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("refused on the command line"),
            "{url}: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
}

async fn create_repository(
    pool: &codeyard_db_core::DbPool,
    owner: i64,
    name: &str,
) -> codeyard_db_core::models::Repository {
    codeyard_db_repos::repositories::records::create_repository(
        pool,
        owner,
        name,
        None,
        false,
        &format!("{name}.git"),
    )
    .await
    .unwrap()
}

/// A repository id deleted before a copy is never issued again, in either direction.
/// Notifications keep a repository's id without a foreign key, so a reissued id would
/// attach their history to an unrelated repository. SQLite-to-SQLite is not a copy this
/// tool performs, so the second hop reads PostgreSQL back into SQLite.
#[tokio::test]
async fn copies_never_reissue_deleted_repository_ids() {
    let _turn = TURNS.lock().await;
    let source = NativeDatabase::new().await;
    let owner = codeyard_db_identity::users::accounts::create_user(&source.pool, "owner", None)
        .await
        .unwrap();
    create_repository(&source.pool, owner.id, "kept").await;
    let deleted = create_repository(&source.pool, owner.id, "deleted").await;
    codeyard_db_repos::repositories::records::delete_repository(&source.pool, owner.id, deleted.id)
        .await
        .unwrap();

    let target = PostgresDatabase::new().await;
    codeyard_db_cluster::copy::copy_database(&source.url, &target.connection_url())
        .await
        .expect("copy SQLite to PostgreSQL");
    let after_copy = create_repository(&target.pool, owner.id, "after-copy").await;
    assert!(
        after_copy.id > deleted.id,
        "PostgreSQL reissued deleted repository id {}",
        deleted.id
    );
    // Deleting the highest id here too leaves only the sequence to remember it.
    codeyard_db_repos::repositories::records::delete_repository(
        &target.pool,
        owner.id,
        after_copy.id,
    )
    .await
    .unwrap();

    let restored = NativeDatabase::new().await;
    codeyard_db_cluster::copy::copy_database(&target.connection_url(), &restored.url)
        .await
        .expect("copy PostgreSQL to SQLite");
    let next = create_repository(&restored.pool, owner.id, "restored").await;
    assert!(
        next.id > after_copy.id,
        "SQLite reissued deleted repository id {}",
        after_copy.id
    );
}

#[tokio::test]
async fn copy_cli_preserves_customized_destination_actions_settings_on_both_engines() {
    let _turn = TURNS.lock().await;
    let sqlite = NativeDatabase::new().await;
    let postgres = PostgresDatabase::new().await;
    sqlx::query("UPDATE actions_settings SET cache_size_gib=3 WHERE id=1")
        .execute(&sqlite.raw)
        .await
        .unwrap();
    sqlx::query("UPDATE actions_settings SET cache_size_gib=2 WHERE id=1")
        .execute(&postgres.raw)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let pg = postgres.connection_url();
    for (source, target) in [(&sqlite.url, &pg), (&pg, &sqlite.url)] {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
                .args(["db", "copy"])
                .current_dir(root.path())
                .env_clear()
                .env("COPY_FROM_DATABASE_URL", source)
                .env("COPY_TO_DATABASE_URL", target)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("copy CLI must finish within its owned timeout")
        .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("actions_settings already has rows"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let pg: i32 = sqlx::query_scalar("SELECT cache_size_gib FROM actions_settings WHERE id=1")
        .fetch_one(&postgres.raw)
        .await
        .unwrap();
    let local: i32 = sqlx::query_scalar("SELECT cache_size_gib FROM actions_settings WHERE id=1")
        .fetch_one(&sqlite.raw)
        .await
        .unwrap();
    assert_eq!((pg, local), (2, 3));
}
