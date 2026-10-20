//! Database layer tests

use crate::support::native_database as native;
use codeyard::backend::db::DbPool;
use uuid::Uuid;

fn with_search_path(database_url: &str, schema: &str) -> String {
    let separator = if database_url.contains('?') { '&' } else { '?' };
    format!("{database_url}{separator}options=-csearch_path%3D{schema}",)
}

// Never load .env, use implicit developer credentials, or accept the application's DATABASE_URL.
fn test_database_url() -> String {
    let value = std::env::var("GITSERV_TEST_DATABASE_URL")
        .expect("DB tests require explicit GITSERV_TEST_DATABASE_URL for gitserv_test");
    assert!(
        safe_test_database_url(&value),
        "fixture URL must name the owned gitserv_test database, without connection options"
    );
    value
}

fn safe_test_database_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "postgres" | "postgresql")
            && url.path() == "/gitserv_test"
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

struct TestDatabase {
    pool: DbPool,
    raw: sqlx::PgPool,
    schema: String,
    database_url: String,
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // This fixture's current-thread runtime is paused while Drop joins the
        // cleanup thread, so connections it just used may not have rolled back
        // yet. The cleanup ends this fixture's own connections instead.
        drop(self.raw.close());
        let schema = self.schema.clone();
        let database_url = self.database_url.clone();
        // A separate runtime permits synchronous, panic-safe cleanup even from
        // within a tokio test. Only this fixture's randomly named schema is dropped.
        let result = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .expect("fixture cleanup runtime")
                .block_on(async move {
                    let options: sqlx::postgres::PgConnectOptions = database_url.parse()?;
                    let admin = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(
                            options.options([("lock_timeout", "5s"), ("statement_timeout", "10s")]),
                        )
                        .await?;
                    // Fixture connections are named after the schema (see setup_test_db).
                    // Ending them rolls back any transaction a failed call left open,
                    // which would otherwise block the DROP.
                    sqlx::query(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1 AND pid <> pg_backend_pid()",
                    )
                    .bind(&schema)
                    .execute(&admin)
                    .await?;
                    let result = sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
                        .execute(&admin)
                        .await;
                    admin.close().await;
                    result.map(|_| ()).map_err(anyhow::Error::from)
                })
        })
        .join();
        if !matches!(result, Ok(Ok(()))) {
            if std::thread::panicking() {
                eprintln!("fixture schema cleanup failed: {}", self.schema);
            } else {
                panic!("fixture schema cleanup failed: {}", self.schema);
            }
        }
    }
}

async fn setup_test_db() -> TestDatabase {
    let database_url = test_database_url();
    let schema = format!("test_{}", Uuid::new_v4().simple());
    let admin = sqlx::postgres::PgPoolOptions::new()
        .connect(&database_url)
        .await
        .expect("required disposable test database is unavailable");
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .expect("create isolated fixture schema");
    // Install the schema guard before scoped connection/migration so failures
    // in required setup also remove this fixture's schema instead of leaking it.
    let mut fixture = TestDatabase {
        pool: admin.clone().into(),
        raw: admin.clone(),
        schema,
        database_url,
    };
    admin.close().await;
    fixture.raw = sqlx::postgres::PgPoolOptions::new()
        .connect(&format!(
            "{}&application_name={}",
            with_search_path(&fixture.database_url, &fixture.schema),
            fixture.schema
        ))
        .await
        .expect("connect to isolated fixture schema");
    fixture.pool = fixture.raw.clone().into();
    codeyard::backend::db::run_migrations(&fixture.pool)
        .await
        .expect("required fixture migration failed");
    fixture
}

fn unique_name(prefix: &str) -> String {
    format!("{}_{}", prefix, Uuid::new_v4().simple())
}

#[test]
fn fixture_rejects_canonical_database_and_supplied_schema_options() {
    assert!(safe_test_database_url(
        "postgres://fixture@localhost/gitserv_test"
    ));
    for value in [
        "postgres://fixture@localhost/gitserv",
        "postgres://fixture@localhost/postgres",
        "postgres://fixture@localhost/gitserv_test?options=-csearch_path=public",
        "https://localhost/gitserv_test",
    ] {
        assert!(!safe_test_database_url(value));
    }
}

#[tokio::test]
async fn test_repeated_migration_preserves_populated_fixture() {
    let fixture = setup_test_db().await;
    let username = unique_name("migration-preservation");
    let user = codeyard::backend::db::users::accounts::create_user(&fixture.pool, &username, None)
        .await
        .expect("create preexisting fixture account");
    codeyard::backend::db::run_migrations(&fixture.pool)
        .await
        .expect("repeat migration on populated isolated schema");
    let preserved = codeyard::backend::db::users::accounts::get_user_by_id(&fixture.pool, user.id)
        .await
        .expect("read preserved fixture account")
        .expect("migration must not remove populated accounts");
    assert_eq!(preserved.username, username);
}

#[tokio::test]
async fn test_migration_installs_file_owner_constraint_in_each_isolated_schema() {
    let first = setup_test_db().await;
    let second = setup_test_db().await;
    for fixture in [&first, &second] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint WHERE conname = 'fk_files_user_id' AND conrelid = 'files'::regclass",
        )
        .fetch_one(&fixture.raw)
        .await
        .expect("read fixture's actual file-owner constraint");
        assert_eq!(count, 1, "file-owner FK missing in {}", fixture.schema);
        let violation = sqlx::query(
            "INSERT INTO files (user_id, filename, original_name, content_type, file_size, sha256_hex, object_key) VALUES (-1, 'fixture', 'fixture', 'text/plain', 0, repeat('0', 64), 'uploads/fixture')",
        )
        .execute(&fixture.raw)
        .await
        .expect_err("fixture file-owner FK must reject a nonexistent account");
        assert_eq!(
            violation
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref(),
            Some("23503"),
        );
    }
}

#[tokio::test]
async fn test_create_user() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let username = unique_name("testuser");
    let email = format!("{}@example.com", username);

    let user = codeyard::backend::db::users::accounts::create_user(&pool, &username, Some(&email))
        .await
        .expect("Failed to create user");

    assert_eq!(user.username, username);
    assert_eq!(user.email, Some(email));
    assert!(!user.is_admin);
}

#[tokio::test]
async fn test_get_user_by_username() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let username = unique_name("findme");

    codeyard::backend::db::users::accounts::create_user(&pool, &username, None)
        .await
        .expect("Failed to create user");

    let found = codeyard::backend::db::users::accounts::get_user_by_username(&pool, &username)
        .await
        .expect("Query failed");

    assert!(found.is_some());
    assert_eq!(found.unwrap().username, username);

    let not_found =
        codeyard::backend::db::users::accounts::get_user_by_username(&pool, "nonexistent")
            .await
            .expect("Query failed");

    assert!(not_found.is_none());
}

/// The lookups that resolve Markdown references, run on whichever engine the pool is.
async fn reference_lookups(pool: &codeyard::backend::db::DbPool) {
    let alice =
        codeyard::backend::db::users::accounts::create_user(pool, &unique_name("refalice"), None)
            .await
            .expect("create user");
    let bob = unique_name("refbob");
    codeyard::backend::db::users::accounts::create_user(pool, &bob, None)
        .await
        .expect("create user");

    let mut found: Vec<String> = codeyard::backend::db::users::accounts::get_users_by_usernames(
        pool,
        &[
            alice.username.clone(),
            bob.clone(),
            "no_such_user".to_owned(),
        ],
    )
    .await
    .expect("username lookup")
    .into_iter()
    .map(|user| user.username)
    .collect();
    found.sort();
    let mut expected = vec![alice.username.clone(), bob.clone()];
    expected.sort();
    assert_eq!(found, expected);
    assert!(
        codeyard::backend::db::users::accounts::get_users_by_usernames(pool, &[])
            .await
            .expect("empty username lookup")
            .is_empty()
    );

    let repo = codeyard::backend::db::repositories::records::create_repository(
        pool,
        alice.id,
        &unique_name("refrepo"),
        None,
        false,
        "/tmp/repos/reference-lookups.git",
    )
    .await
    .expect("create repository");
    let counted = codeyard::backend::db::discussions::last_discussion_numbers(pool, &[repo.id])
        .await
        .expect("counter lookup");
    assert!(
        counted.is_empty(),
        "a repository without items has no counter"
    );
    for title in ["first", "second"] {
        codeyard::backend::db::issues::create_issue(pool, repo.id, alice.id, title, "body")
            .await
            .expect("create issue");
    }
    let counted = codeyard::backend::db::discussions::last_discussion_numbers(
        pool,
        &[repo.id, repo.id + 1_000_000],
    )
    .await
    .expect("counter lookup");
    assert_eq!(counted, vec![(repo.id, 2)]);
}

#[tokio::test]
async fn test_reference_lookups_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    reference_lookups(&database.pool).await;
}

#[tokio::test]
async fn test_reference_lookups_on_postgres() {
    let fixture = setup_test_db().await;
    reference_lookups(&fixture.pool).await;
}

#[tokio::test]
async fn test_create_repository() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let owner_name = unique_name("repoowner");
    let repo_name = unique_name("myrepo");

    let user = codeyard::backend::db::users::accounts::create_user(&pool, &owner_name, None)
        .await
        .expect("Failed to create user");

    let repo = codeyard::backend::db::repositories::records::create_repository(
        &pool,
        user.id,
        &repo_name,
        Some("A test repository"),
        false,
        &format!("/tmp/repos/{}/{}.git", owner_name, repo_name),
    )
    .await
    .expect("Failed to create repository");

    assert_eq!(repo.name, repo_name);
    assert_eq!(repo.owner_id(), user.id);
    assert!(!repo.is_private);
}

#[tokio::test]
async fn test_get_repository() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let owner_name = unique_name("owner");
    let repo_name = unique_name("testrepo");

    let user = codeyard::backend::db::users::accounts::create_user(&pool, &owner_name, None)
        .await
        .expect("Failed to create user");

    codeyard::backend::db::repositories::records::create_repository(
        &pool,
        user.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{}.git", repo_name),
    )
    .await
    .expect("Failed to create repo");

    let found = codeyard::backend::db::repositories::records::get_repository(
        &pool,
        &owner_name,
        &repo_name,
    )
    .await
    .expect("Query failed");

    assert!(found.is_some());
    assert_eq!(found.unwrap().name, repo_name);

    let not_found =
        codeyard::backend::db::repositories::records::get_repository(&pool, "owner", "nonexistent")
            .await
            .expect("Query failed");

    assert!(not_found.is_none());
}

#[tokio::test]
async fn test_ssh_key_operations() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let username = unique_name("keyowner");

    let user = codeyard::backend::db::users::accounts::create_user(&pool, &username, None)
        .await
        .expect("Failed to create user");

    let public_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl test@example.com";

    let key = codeyard::backend::db::users::ssh_keys::add_ssh_key(
        &pool,
        user.id,
        "my-laptop",
        public_key,
    )
    .await
    .expect("Failed to add SSH key");

    assert_eq!(key.name, "my-laptop");
    assert_eq!(key.key_type, "ssh-ed25519");
    assert!(key.fingerprint.starts_with("SHA256:"));

    let keys = codeyard::backend::db::users::ssh_keys::list_user_keys(&pool, user.id)
        .await
        .expect("Failed to list keys");

    assert_eq!(keys.len(), 1);

    let deleted = codeyard::backend::db::users::ssh_keys::delete_ssh_key(&pool, key.id)
        .await
        .expect("Failed to delete key");

    assert!(deleted);
}

#[tokio::test]
async fn test_access_control() {
    use codeyard::backend::db::models::AccessLevel;

    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let owner_name = unique_name("owner");
    let stranger_name = unique_name("stranger");
    let public_repo_name = unique_name("public-repo");
    let private_repo_name = unique_name("private-repo");

    let owner = codeyard::backend::db::users::accounts::create_user(&pool, &owner_name, None)
        .await
        .expect("Failed to create owner");

    let stranger = codeyard::backend::db::users::accounts::create_user(&pool, &stranger_name, None)
        .await
        .expect("Failed to create stranger");

    let public_repo = codeyard::backend::db::repositories::records::create_repository(
        &pool,
        owner.id,
        &public_repo_name,
        None,
        false,
        &format!("/tmp/{}.git", public_repo_name),
    )
    .await
    .expect("Failed to create repo");

    let private_repo = codeyard::backend::db::repositories::records::create_repository(
        &pool,
        owner.id,
        &private_repo_name,
        None,
        true,
        &format!("/tmp/{}.git", private_repo_name),
    )
    .await
    .expect("Failed to create repo");

    let owner_access = codeyard::backend::db::repositories::access::get_user_repo_access(
        &pool,
        owner.id,
        public_repo.id,
    )
    .await
    .expect("Query failed");
    assert_eq!(owner_access, AccessLevel::Owner);

    let stranger_public = codeyard::backend::db::repositories::access::get_user_repo_access(
        &pool,
        stranger.id,
        public_repo.id,
    )
    .await
    .expect("Query failed");
    assert_eq!(stranger_public, AccessLevel::Read);

    let stranger_private = codeyard::backend::db::repositories::access::get_user_repo_access(
        &pool,
        stranger.id,
        private_repo.id,
    )
    .await
    .expect("Query failed");
    assert_eq!(stranger_private, AccessLevel::None);
}

#[tokio::test]
async fn test_private_repository_grants_are_scoped_and_revocable() {
    use codeyard::backend::db::{self, models::AccessLevel};
    let fixture = setup_test_db().await;
    let owner = db::users::accounts::create_user(&fixture.pool, &unique_name("access-owner"), None)
        .await
        .expect("create fixture owner");
    let collaborator =
        db::users::accounts::create_user(&fixture.pool, &unique_name("access-member"), None)
            .await
            .expect("create fixture collaborator");
    let unrelated =
        db::users::accounts::create_user(&fixture.pool, &unique_name("access-unrelated"), None)
            .await
            .expect("create unrelated fixture account");
    let mut repositories = Vec::new();
    for _ in 0..2 {
        repositories.push(
            db::repositories::records::create_repository(
                &fixture.pool,
                owner.id,
                &unique_name("scoped-private"),
                None,
                true,
                "/unused-scoped-fixture.git",
            )
            .await
            .expect("create private fixture repository"),
        );
    }
    db::repositories::access::grant_repo_access(
        &fixture.pool,
        collaborator.id,
        repositories[0].id,
        AccessLevel::Write,
    )
    .await
    .expect("grant fixture access");
    assert_eq!(
        db::repositories::access::get_user_repo_access(
            &fixture.pool,
            collaborator.id,
            repositories[0].id
        )
        .await
        .unwrap(),
        AccessLevel::Write
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(
            &fixture.pool,
            collaborator.id,
            repositories[1].id
        )
        .await
        .unwrap(),
        AccessLevel::None
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(
            &fixture.pool,
            unrelated.id,
            repositories[0].id
        )
        .await
        .unwrap(),
        AccessLevel::None
    );
    assert!(db::repositories::access::revoke_repo_access(
        &fixture.pool,
        collaborator.id,
        repositories[0].id
    )
    .await
    .unwrap());
    assert_eq!(
        db::repositories::access::get_user_repo_access(
            &fixture.pool,
            collaborator.id,
            repositories[0].id
        )
        .await
        .unwrap(),
        AccessLevel::None
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(&fixture.pool, owner.id, repositories[0].id)
            .await
            .unwrap(),
        AccessLevel::Owner
    );
}

#[tokio::test]
async fn test_session_management() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let username = unique_name("sessionuser");

    let user = codeyard::backend::db::users::accounts::create_user(&pool, &username, None)
        .await
        .expect("Failed to create user");

    let session_id = format!("test-session-{}", Uuid::new_v4().simple());
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);

    codeyard::backend::db::users::sessions::create_session(
        &pool,
        &session_id,
        user.id,
        None,
        None,
        expires_at,
    )
    .await
    .expect("Failed to create session");

    let found_user = codeyard::backend::db::users::sessions::get_session_user(&pool, &session_id)
        .await
        .expect("Query failed");

    assert!(found_user.is_some());
    assert_eq!(found_user.unwrap().username, username);

    codeyard::backend::db::users::sessions::delete_session(&pool, &session_id)
        .await
        .expect("Failed to delete session");

    let deleted = codeyard::backend::db::users::sessions::get_session_user(&pool, &session_id)
        .await
        .expect("Query failed");

    assert!(deleted.is_none());
}

#[tokio::test]
async fn test_branch_protection_and_commit_statuses() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let owner_name = unique_name("bp-owner");
    let repo_name = unique_name("bp-repo");

    let owner = codeyard::backend::db::users::accounts::create_user(&pool, &owner_name, None)
        .await
        .expect("Failed to create user");

    let repo = codeyard::backend::db::repositories::records::create_repository(
        &pool,
        owner.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{}.git", repo_name),
    )
    .await
    .expect("Failed to create repository");

    let rule = codeyard::backend::db::repositories::protection::upsert_branch_protection_rule(
        &pool,
        codeyard::backend::db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: repo.id,
            pattern: "main",
            require_pull_request: true,
            required_approving_reviews: 2,
            dismiss_stale_reviews: true,
            require_status_checks: true,
            required_status_checks: vec!["ci/test".to_string(), "lint".to_string()],
            require_up_to_date_branch: true,
            ..Default::default()
        },
    )
    .await
    .expect("Failed to upsert branch protection rule");

    assert!(rule.require_pull_request);
    assert_eq!(rule.required_approving_reviews, 2);
    assert!(rule.dismiss_stale_reviews);
    assert!(rule.require_status_checks);
    assert_eq!(rule.required_status_checks, vec!["ci/test", "lint"]);
    assert!(rule.require_up_to_date_branch);

    let sha = "0123456789012345678901234567890123456789";

    let pending = codeyard::backend::db::repositories::protection::upsert_commit_status(
        &pool,
        codeyard::backend::db::repositories::protection::CommitStatusUpsert {
            repo_id: repo.id,
            sha,
            context: "ci/test",
            state: "pending",
            description: Some("Running tests"),
            target_url: None,
            creator_id: Some(owner.id),
        },
    )
    .await
    .expect("Failed to create pending status");
    assert_eq!(pending.state, "pending");

    let success = codeyard::backend::db::repositories::protection::upsert_commit_status(
        &pool,
        codeyard::backend::db::repositories::protection::CommitStatusUpsert {
            repo_id: repo.id,
            sha,
            context: "ci/test",
            state: "success",
            description: Some("All tests passed"),
            target_url: Some("https://ci.example.com/runs/1"),
            creator_id: Some(owner.id),
        },
    )
    .await
    .expect("Failed to update status");
    assert_eq!(success.state, "success");

    let statuses =
        codeyard::backend::db::repositories::protection::list_commit_statuses(&pool, repo.id, sha)
            .await
            .expect("Failed to list statuses");

    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].context, "ci/test");
    assert_eq!(statuses[0].state, "success");
}

#[tokio::test]
async fn test_ci_queue_claim_heartbeat_finish_cancel_and_stale() {
    use codeyard::backend::db;
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();
    let owner = db::users::accounts::create_user(&pool, &unique_name("ci-owner"), None)
        .await
        .unwrap();
    // Workflows run on the server's machines: only admins or trusted users queue runs.
    sqlx::query("UPDATE users SET is_admin = TRUE WHERE id = $1")
        .bind(owner.id)
        .execute(&fixture.raw)
        .await
        .unwrap();
    let repo_name = unique_name("ci-repo");
    let repo = db::repositories::records::create_repository(
        &pool,
        owner.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{repo_name}.git"),
    )
    .await
    .unwrap();
    let sha = "0123456789012345678901234567890123456789";
    let blobs = serde_json::json!({".github/workflows/ci.yml": sha});
    let new_run = |pipeline: &'static str, environment: Option<&'static str>| db::ci::NewCiRun {
        repo_id: repo.id,
        triggered_by_id: owner.id,
        pipeline,
        event: "push",
        pull_request_id: None,
        branch: "main",
        head_sha: sha,
        workflow_path: ".github/workflows/ci.yml",
        workflow_blobs: &blobs,
        workflow_inputs: None,
        environment,
        manual: false,
    };

    let run = db::ci::enqueue_ci_run(&pool, new_run("full", Some("production")), &[])
        .await
        .unwrap();
    assert_eq!(run.status, "queued");
    let claimed = db::ci::claim_ci_run(&pool, "node-a/1", "podman", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, run.id);
    assert_eq!(claimed.status, "running");
    assert_eq!(claimed.worker.as_deref(), Some("node-a/1"));
    assert!(
        db::ci::claim_ci_run(&pool, "node-b/1", "local", &[])
            .await
            .unwrap()
            .is_none(),
        "one run, one worker"
    );
    assert_eq!(
        db::ci::heartbeat_ci_run(&pool, run.id, "node-a/1")
            .await
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        db::ci::heartbeat_ci_run(&pool, run.id, "node-b/1")
            .await
            .unwrap(),
        None
    );

    db::ci::append_ci_logs(
        &pool,
        run.id,
        "node-a/1",
        &[
            codeyard::backend::ci::queue::LogLine {
                stage: "ci.yml".into(),
                line: "running tests".into(),
                level: "info".into(),
            },
            codeyard::backend::ci::queue::LogLine {
                stage: "ci.yml".into(),
                line: "warning: flaky test".into(),
                level: "warn".into(),
            },
        ],
    )
    .await
    .unwrap();
    let logs = db::ci::list_ci_logs(&pool, run.id, 50).await.unwrap();
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[0].line, "running tests");
    assert_eq!(logs[1].level, "warn");

    assert!(
        db::ci::finish_ci_run(&pool, run.id, "node-b/1", "success", None)
            .await
            .unwrap()
            .is_none()
    );
    let (finished, notifications) =
        db::ci::finish_ci_run(&pool, run.id, "node-a/1", "success", None)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(finished.status, "success");
    assert!(notifications.is_empty(), "a successful run tells no one");
    assert!(finished.completed_at.is_some());
    let deployment = db::ci::get_ci_deployment_by_run_id(&pool, run.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (deployment.environment.as_str(), deployment.status.as_str()),
        ("production", "success")
    );
    let statuses = db::repositories::protection::list_commit_statuses(&pool, repo.id, sha)
        .await
        .unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(
        (statuses[0].context.as_str(), statuses[0].state.as_str()),
        ("gitserv/workflow/.github/workflows/ci.yml", "success")
    );
    assert_eq!(
        db::ci::list_ci_runs_for_repo(&pool, repo.id, 20)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db::ci::list_ci_runs_for_user(&pool, owner.id, 20)
            .await
            .unwrap()
            .len(),
        1
    );

    // Cancelling a queued run finishes it; cancelling a running one asks its worker.
    let queued = db::ci::enqueue_ci_run(&pool, new_run("ci", None), &[])
        .await
        .unwrap();
    assert!(
        db::ci::request_ci_cancellation(&pool, queued.run_id, owner.id)
            .await
            .unwrap()
            .0
    );
    let cancelled = db::ci::get_ci_run_by_run_id(&pool, queued.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    let running = db::ci::enqueue_ci_run(&pool, new_run("ci", None), &[])
        .await
        .unwrap();
    db::ci::claim_ci_run(&pool, "node-a/1", "podman", &[])
        .await
        .unwrap()
        .unwrap();
    assert!(
        db::ci::request_ci_cancellation(&pool, running.run_id, owner.id)
            .await
            .unwrap()
            .0
    );
    assert_eq!(
        db::ci::heartbeat_ci_run(&pool, running.id, "node-a/1")
            .await
            .unwrap(),
        Some(true)
    );

    // A run whose worker stopped sending heartbeats is failed, not retried.
    sqlx::query("UPDATE ci_runs SET heartbeat_at = heartbeat_at - INTERVAL '1 hour' WHERE id = $1")
        .bind(running.id)
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(db::ci::fail_stale_ci_runs(&pool, 90).await.unwrap().0, 1);
    let failed = db::ci::get_ci_run_by_run_id(&pool, running.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, "failed");

    // Non-admin, untrusted users can't queue runs.
    let stranger = db::users::accounts::create_user(&pool, &unique_name("ci-stranger"), None)
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(
        &pool,
        stranger.id,
        repo.id,
        codeyard::backend::db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let mut untrusted = new_run("ci", None);
    untrusted.triggered_by_id = stranger.id;
    assert!(db::ci::enqueue_ci_run(&pool, untrusted, &[]).await.is_err());
    let mut trusted = new_run("ci", None);
    trusted.triggered_by_id = stranger.id;
    db::ci::enqueue_ci_run(&pool, trusted, &[stranger.id])
        .await
        .unwrap();
}

#[tokio::test]
async fn test_fingerprint_calculation() {
    let public_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl test@example.com";

    let fingerprint = codeyard::backend::db::users::ssh_keys::calculate_fingerprint(public_key)
        .expect("Failed to calculate fingerprint");

    assert!(fingerprint.starts_with("SHA256:"));
    assert!(fingerprint.len() > 10);
}

#[tokio::test]
async fn test_access_level_ordering() {
    use codeyard::backend::db::models::AccessLevel;

    assert!(AccessLevel::Read > AccessLevel::None);
    assert!(AccessLevel::Write > AccessLevel::Read);
    assert!(AccessLevel::Admin > AccessLevel::Write);
    assert!(AccessLevel::Owner > AccessLevel::Admin);

    assert!(AccessLevel::Read.can_read());
    assert!(!AccessLevel::Read.can_write());

    assert!(AccessLevel::Write.can_read());
    assert!(AccessLevel::Write.can_write());
    assert!(!AccessLevel::Write.can_admin());

    assert!(AccessLevel::Owner.can_read());
    assert!(AccessLevel::Owner.can_write());
    assert!(AccessLevel::Owner.can_admin());
    assert!(AccessLevel::Owner.is_owner());
}

#[tokio::test]
async fn test_node_configuration_upsert_and_get() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let initial =
        codeyard::backend::db::node_config::upsert_node_configuration(&pool, "private", true)
            .await
            .expect("Failed to upsert node config");
    assert_eq!(initial.id, 1);
    assert_eq!(initial.node_mode, "private");
    assert!(initial.setup_completed);

    let updated =
        codeyard::backend::db::node_config::upsert_node_configuration(&pool, "public", true)
            .await
            .expect("Failed to update node config");
    assert_eq!(updated.id, 1);
    assert_eq!(updated.node_mode, "public");

    let fetched = codeyard::backend::db::node_config::get_node_configuration(&pool)
        .await
        .expect("Failed to fetch node config")
        .expect("Node config should exist");
    assert_eq!(fetched.id, 1);
    assert_eq!(fetched.node_mode, "public");
    assert!(fetched.setup_completed);
}

#[tokio::test]
async fn test_user_password_flags_flow() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let username = unique_name("flaguser");
    let email = format!("{}@example.com", username);

    let created = codeyard::backend::db::users::accounts::create_user_with_password_and_flags(
        &pool,
        &username,
        Some(&email),
        "initial-hash",
        true,
        true,
    )
    .await
    .expect("Failed to create user with flags");

    assert!(created.is_admin);
    assert!(created.must_change_password);

    codeyard::backend::db::users::accounts::update_user_password(
        &pool, created.id, "new-hash", false,
    )
    .await
    .expect("Failed to update password");

    let fetched = codeyard::backend::db::users::accounts::get_user_by_id(&pool, created.id)
        .await
        .expect("Failed to fetch user")
        .expect("User should exist");
    assert_eq!(fetched.password_hash.as_deref(), Some("new-hash"));
    assert!(!fetched.must_change_password);
    assert!(fetched.is_admin);
}

#[tokio::test]
async fn test_token_creation_refuses_session_revoked_by_password_change_on_postgres() {
    let fixture = setup_test_db().await;
    token_session_revocation_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_token_creation_refuses_session_revoked_by_password_change_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    token_session_revocation_contract(&database.pool).await;
    database.close().await;
}

/// A token request whose session a password change revoked after the session check creates
/// no token. The session check and the insert are one transaction or statement, so the
/// change either precedes the insert or the insert is refused.
async fn token_session_revocation_contract(pool: &DbPool) {
    use codeyard::backend::{
        db::users::{accounts, sessions, transitions},
        identity::tokens::{self, TokenError, TokenInput},
    };
    let user = accounts::create_user_with_password_and_flags(
        pool,
        &unique_name("token-race"),
        None,
        "old-hash",
        false,
        false,
    )
    .await
    .expect("Failed to create user");
    let session = format!("test-session-{}", Uuid::new_v4().simple());
    sessions::create_session(
        pool,
        &session,
        user.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .expect("Failed to create session");
    assert!(tokens::create(
        pool,
        user.id,
        Some(&session),
        TokenInput {
            name: "before",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .is_ok());
    // The request has passed its session check. The password change now commits.
    assert!(sessions::get_session_account(pool, &session)
        .await
        .unwrap()
        .is_some());
    assert!(transitions::compare_exchange_password(
        pool, user.id, "old-hash", "new-hash", true, true, None
    )
    .await
    .unwrap()
    .is_some());
    // The insert that resumes afterwards must not create a usable token.
    assert!(matches!(
        tokens::create(
            pool,
            user.id,
            Some(&session),
            TokenInput {
                name: "after",
                scope: "write",
                expires_at: None,
            },
        )
        .await,
        Err(TokenError::Denied)
    ));
    assert!(
        codeyard::backend::db::users::tokens::list_tokens(pool, user.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_signup_requests_flow() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();

    let reviewer_name = unique_name("reviewer");
    let reviewer = codeyard::backend::db::users::accounts::create_user_with_password_and_flags(
        &pool,
        &reviewer_name,
        None,
        "reviewer-hash",
        true,
        false,
    )
    .await
    .expect("Failed to create reviewer");
    codeyard::backend::db::node_config::upsert_node_configuration(&pool, "private", true)
        .await
        .expect("review requires a configured private node and current administrator");

    let pending_name = unique_name("pendinguser");
    let pending_email = format!("{}@example.com", pending_name);
    let pending = codeyard::backend::db::users::signup_requests::create_signup_request(
        &pool,
        &pending_name,
        Some(&pending_email),
        "pending-hash",
    )
    .await
    .expect("Failed to create signup request");

    let found_by_identity =
        codeyard::backend::db::users::signup_requests::get_pending_signup_request_by_identity(
            &pool,
            &pending_name,
            None,
        )
        .await
        .expect("Failed to lookup by username")
        .expect("Pending request should be found by username");
    assert_eq!(found_by_identity.id, pending.id);

    let found_by_email_case =
        codeyard::backend::db::users::signup_requests::get_pending_signup_request_by_identity(
            &pool,
            "nonexistent",
            Some(&pending_email.to_uppercase()),
        )
        .await
        .expect("Failed to lookup by email")
        .expect("Pending request should be found by email");
    assert_eq!(found_by_email_case.id, pending.id);

    let approved = codeyard::backend::db::users::signup_requests::approve_signup_request(
        &pool,
        pending.id,
        reviewer.id,
    )
    .await
    .expect("Failed to approve signup request");
    assert_eq!(approved.username, pending_name);
    assert_eq!(approved.email.as_deref(), Some(pending_email.as_str()));
    assert!(!approved.must_change_password);

    let pending_after_approval =
        codeyard::backend::db::users::signup_requests::list_signup_requests(
            &pool, "pending", 100, 0, 0,
        )
        .await
        .expect("Failed to list pending requests");
    assert!(!pending_after_approval.iter().any(|r| r.id == pending.id));

    let second_name = unique_name("rejectuser");
    let rejected = codeyard::backend::db::users::signup_requests::create_signup_request(
        &pool,
        &second_name,
        None,
        "reject-hash",
    )
    .await
    .expect("Failed to create second signup request");

    codeyard::backend::db::users::signup_requests::reject_signup_request(
        &pool,
        rejected.id,
        reviewer.id,
    )
    .await
    .expect("Failed to reject signup request");

    let pending_after_reject = codeyard::backend::db::users::signup_requests::list_signup_requests(
        &pool, "pending", 100, 0, 0,
    )
    .await
    .expect("Failed to list pending after reject");
    assert!(!pending_after_reject.iter().any(|r| r.id == rejected.id));

    let rejected_items = codeyard::backend::db::users::signup_requests::list_signup_requests(
        &pool, "rejected", 100, 0, 0,
    )
    .await
    .expect("Failed to list rejected requests");
    assert!(rejected_items.iter().any(|r| r.id == rejected.id));
}

#[tokio::test]
async fn test_repository_delete_removes_row_history_and_frees_name() {
    let fixture = setup_test_db().await;
    let pool = fixture.pool.clone();
    let owner = codeyard::backend::db::users::accounts::create_user(
        &pool,
        &unique_name("storage-owner"),
        None,
    )
    .await
    .unwrap();
    let repo_name = unique_name("storage-repo");
    let disk_path = format!("/tmp/{repo_name}.git");
    let repo = codeyard::backend::db::repositories::records::create_repository(
        &pool, owner.id, &repo_name, None, false, &disk_path,
    )
    .await
    .unwrap();
    codeyard::backend::db::repositories::records::insert_repo_storage_snapshot(
        &pool, repo.id, 2048,
    )
    .await
    .unwrap();
    let removed =
        codeyard::backend::db::repositories::records::delete_repository(&pool, owner.id, repo.id)
            .await
            .unwrap();
    assert_eq!(removed.disk_path, disk_path);
    assert!(
        codeyard::backend::db::repositories::records::get_repository_by_id(&pool, repo.id)
            .await
            .unwrap()
            .is_none()
    );
    let history: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM repo_storage_history WHERE repo_id = $1")
            .bind(repo.id)
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(history, 0);
    // The name can be used again.
    codeyard::backend::db::repositories::records::create_repository(
        &pool, owner.id, &repo_name, None, false, &disk_path,
    )
    .await
    .unwrap();
    // Only someone with admin access to the repository can delete it.
    let other =
        codeyard::backend::db::users::accounts::create_user(&pool, &unique_name("not-owner"), None)
            .await
            .unwrap();
    let again = codeyard::backend::db::repositories::records::get_repository(
        &pool,
        &owner.username,
        &repo_name,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        codeyard::backend::db::repositories::records::delete_repository(&pool, other.id, again.id)
            .await
            .is_err()
    );
}

/// Editing a pull request needs the latest revision, and a merge is recorded once.
async fn pull_request_edit_contract(pool: &DbPool) {
    use codeyard::backend::db::{self, discussions::DiscussionEdit};
    let owner = db::users::accounts::create_user(pool, &unique_name("pr-owner"), None)
        .await
        .unwrap();
    let repo_name = unique_name("pr-repo");
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{repo_name}.git"),
    )
    .await
    .unwrap();
    let (head, base, merged_sha) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let pr = db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
            head_repo_id: None,
            repo_id: repo.id,
            author_id: owner.id,
            title: "first",
            body: "body",
            head_branch: "feature",
            head_sha: &head,
            base_branch: "main",
            base_sha: &base,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let edit = |expected_state, expected_updated, title, state| DiscussionEdit {
        actor_id: owner.id,
        expected_state,
        expected_updated,
        title,
        body: None,
        state,
    };
    let edited = db::pull_requests::records::update_pull_request(
        pool,
        pr.id,
        edit("open", pr.updated_at, Some("second"), None),
        None,
    )
    .await
    .unwrap()
    .expect("the owner can edit an open pull request");
    assert_eq!(edited.title, "second");
    // The revision the editor started from is out of date now.
    assert!(db::pull_requests::records::update_pull_request(
        pool,
        pr.id,
        edit("open", pr.updated_at, Some("stale"), None),
        None,
    )
    .await
    .unwrap()
    .is_none());
    assert!(db::pull_requests::records::mark_pull_requests_merged(
        pool,
        std::slice::from_ref(&edited),
        owner.id,
        &merged_sha
    )
    .await
    .unwrap());
    // It can only be merged once, and a merged pull request cannot be reopened.
    assert!(!db::pull_requests::records::mark_pull_requests_merged(
        pool,
        std::slice::from_ref(&edited),
        owner.id,
        &merged_sha
    )
    .await
    .unwrap());
    let merged = db::pull_requests::records::get_pull_request_by_id(pool, pr.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.state, "merged");
    assert_eq!(
        merged.merge_commit_sha.as_deref(),
        Some(merged_sha.as_str())
    );
    assert!(db::pull_requests::records::update_pull_request(
        pool,
        pr.id,
        edit("merged", merged.updated_at, None, Some("open")),
        None,
    )
    .await
    .unwrap()
    .is_none());
}

#[tokio::test]
async fn test_pull_request_edit_and_merge_on_postgres() {
    let fixture = setup_test_db().await;
    pull_request_edit_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_pull_request_edit_and_merge_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    pull_request_edit_contract(&database.pool).await;
    database.close().await;
}

/// Open pull requests move with their branches; closed and merged ones stay put.
async fn pull_request_revision_contract(pool: &DbPool) {
    use codeyard::backend::db::{
        self, discussions::DiscussionEdit, pull_requests::records::PullRequestRevisions,
    };
    let owner = db::users::accounts::create_user(pool, &unique_name("rev-owner"), None)
        .await
        .unwrap();
    let repo_name = unique_name("rev-repo");
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{repo_name}.git"),
    )
    .await
    .unwrap();
    let sha = |c: char| c.to_string().repeat(40);
    let open = |branch: &str, head: char| {
        let (branch, head) = (branch.to_owned(), sha(head));
        async move {
            db::pull_requests::records::create_pull_request(
                pool,
                db::pull_requests::records::NewPullRequest {
                    head_repo_id: None,
                    repo_id: repo.id,
                    author_id: owner.id,
                    title: "revisions",
                    body: "",
                    head_branch: &branch,
                    head_sha: &head,
                    base_branch: "main",
                    base_sha: &sha('2'),
                    is_draft: false,
                },
            )
            .await
            .unwrap()
        }
    };
    let first = open("feature-a", '1').await;
    let second = open("feature-b", '3').await;
    let closed = open("feature-c", '4').await;
    let merged = open("feature-d", '5').await;
    let close = DiscussionEdit {
        actor_id: owner.id,
        expected_state: "open",
        expected_updated: closed.updated_at,
        title: None,
        body: None,
        state: Some("closed"),
    };
    let closed = db::pull_requests::records::update_pull_request(pool, closed.id, close, None)
        .await
        .unwrap()
        .expect("the owner can close it");
    assert!(db::pull_requests::records::mark_pull_requests_merged(
        pool,
        std::slice::from_ref(&merged),
        owner.id,
        &sha('f')
    )
    .await
    .unwrap());

    // Only open pull requests are listed, with the commits they record.
    let listed = db::pull_requests::records::list_open_pull_request_revisions(pool, repo.id)
        .await
        .unwrap();
    assert_eq!(
        listed,
        vec![
            PullRequestRevisions {
                id: first.id,
                repo_id: repo.id,
                number: first.number,
                author_id: owner.id,
                head_repo_id: Some(repo.id),
                head_branch: "feature-a".into(),
                head_sha: sha('1'),
                base_branch: "main".into(),
                base_sha: sha('2'),
                stack_id: None,
            },
            PullRequestRevisions {
                id: second.id,
                repo_id: repo.id,
                number: second.number,
                author_id: owner.id,
                head_repo_id: Some(repo.id),
                head_branch: "feature-b".into(),
                head_sha: sha('3'),
                base_branch: "main".into(),
                base_sha: sha('2'),
                stack_id: None,
            },
        ]
    );

    // Head and base moves use independent compare-and-swaps, so contention on
    // one revision cannot prevent the other revision from advancing.
    assert!(!db::pull_requests::records::move_open_pull_request_head(
        pool,
        first.id,
        &sha('9'),
        &sha('6'),
    )
    .await
    .unwrap());
    assert!(db::pull_requests::records::move_open_pull_request_head(
        pool,
        first.id,
        &sha('1'),
        &sha('6'),
    )
    .await
    .unwrap());
    assert!(db::pull_requests::records::move_open_pull_request_base(
        pool,
        first.id,
        &sha('2'),
        &sha('7'),
        "main",
        None,
    )
    .await
    .unwrap());
    let moved = db::pull_requests::records::get_pull_request_by_id(pool, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (moved.head_sha.as_str(), moved.base_sha.as_str()),
        (sha('6').as_str(), sha('7').as_str())
    );
    assert!(
        moved.updated_at > first.updated_at,
        "the revision moves forward"
    );
    // A refresh that read another base branch cannot move this base sha, even when the sha still
    // matches; a refresh that read the current base branch can.
    assert!(!db::pull_requests::records::move_open_pull_request_base(
        pool,
        first.id,
        &sha('7'),
        &sha('8'),
        "develop",
        None,
    )
    .await
    .unwrap());
    assert!(db::pull_requests::records::move_open_pull_request_base(
        pool,
        first.id,
        &sha('7'),
        &sha('8'),
        "main",
        None,
    )
    .await
    .unwrap());
    // Neither move can be applied twice with a stale expected revision.
    assert!(!db::pull_requests::records::move_open_pull_request_head(
        pool,
        first.id,
        &sha('1'),
        &sha('8'),
    )
    .await
    .unwrap());
    assert!(!db::pull_requests::records::move_open_pull_request_base(
        pool,
        first.id,
        &sha('2'),
        &sha('8'),
        "main",
        None,
    )
    .await
    .unwrap());
    // A form opened before the move is out of date now.
    let stale = DiscussionEdit {
        actor_id: owner.id,
        expected_state: "open",
        expected_updated: first.updated_at,
        title: Some("stale"),
        body: None,
        state: None,
    };
    assert!(
        db::pull_requests::records::update_pull_request(pool, first.id, stale, None)
            .await
            .unwrap()
            .is_none()
    );
    // Closed and merged pull requests never move.
    for pull in [&closed, &merged] {
        assert!(!db::pull_requests::records::move_open_pull_request_head(
            pool,
            pull.id,
            &pull.head_sha,
            &sha('8'),
        )
        .await
        .unwrap());
        assert!(!db::pull_requests::records::move_open_pull_request_base(
            pool,
            pull.id,
            &pull.base_sha,
            &sha('8'),
            "main",
            None,
        )
        .await
        .unwrap());
    }
    for id in [closed.id, merged.id] {
        let row = db::pull_requests::records::get_pull_request_by_id(pool, id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(row.head_sha, sha('8'));
    }

    // Merging records the commits that were merged, even if the pull request moved
    // after they were read.
    let mut merging = moved.clone();
    merging.head_sha = sha('a');
    merging.base_sha = sha('b');
    assert!(db::pull_requests::records::mark_pull_requests_merged(
        pool,
        std::slice::from_ref(&merging),
        owner.id,
        &sha('c')
    )
    .await
    .unwrap());
    let done = db::pull_requests::records::get_pull_request_by_id(pool, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.state, "merged");
    assert_eq!(
        (
            done.head_sha.as_str(),
            done.base_sha.as_str(),
            done.merge_commit_sha.as_deref()
        ),
        (
            sha('a').as_str(),
            sha('b').as_str(),
            Some(sha('c').as_str())
        )
    );
    assert_eq!(
        db::pull_requests::records::list_open_pull_request_revisions(pool, repo.id)
            .await
            .unwrap()
            .len(),
        1,
        "only the second pull request is still open"
    );

    // Parent-row serialization permits exactly one concurrent open PR for an
    // effective head repository/head branch/base branch tuple.
    let spawn_duplicate = |title: &'static str| {
        let pool = pool.clone();
        let repo_id = repo.id;
        let author_id = owner.id;
        tokio::spawn(async move {
            let head_sha = "d".repeat(40);
            let base_sha = "e".repeat(40);
            db::pull_requests::records::create_pull_request(
                &pool,
                db::pull_requests::records::NewPullRequest {
                    head_repo_id: Some(repo_id),
                    repo_id,
                    author_id,
                    title,
                    body: "",
                    head_branch: "duplicate-topic",
                    head_sha: &head_sha,
                    base_branch: "main",
                    base_sha: &base_sha,
                    is_draft: false,
                },
            )
            .await
        })
    };
    let (left, right) = tokio::join!(spawn_duplicate("left"), spawn_duplicate("right"));
    let mut winner = None;
    let mut refused = 0;
    for result in [left.unwrap(), right.unwrap()] {
        match result {
            Ok(pull) => winner = Some(pull),
            Err(error) => {
                assert!(error.is::<db::pull_requests::records::DuplicateOpenPullRequest>());
                refused += 1;
            }
        }
    }
    assert_eq!(refused, 1);
    let winner = winner.expect("one concurrent creator must win");
    let closed_duplicate = db::pull_requests::records::update_pull_request(
        pool,
        winner.id,
        DiscussionEdit {
            actor_id: owner.id,
            expected_state: "open",
            expected_updated: winner.updated_at,
            title: None,
            body: None,
            state: Some("closed"),
        },
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(closed_duplicate.state, "closed");
    let head_sha = sha('d');
    let base_sha = sha('e');
    let reopened = db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
            head_repo_id: Some(repo.id),
            repo_id: repo.id,
            author_id: owner.id,
            title: "reopened tuple",
            body: "",
            head_branch: "duplicate-topic",
            head_sha: &head_sha,
            base_branch: "main",
            base_sha: &base_sha,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    assert!(reopened.number > winner.number);
    // Reopening the closed one would give the same branches two open pull requests.
    let refused = db::pull_requests::records::update_pull_request(
        pool,
        winner.id,
        DiscussionEdit {
            actor_id: owner.id,
            expected_state: "closed",
            expected_updated: closed_duplicate.updated_at,
            title: None,
            body: None,
            state: Some("open"),
        },
        None,
    )
    .await
    .unwrap_err();
    let duplicate = refused
        .downcast::<db::pull_requests::records::DuplicateOpenPullRequest>()
        .unwrap();
    assert_eq!(duplicate.number, reopened.number);
}

/// A merge marks the pull request only while it still targets the branch the merge went into. A
/// base change made after the merge was published leaves the pull request open.
async fn pull_request_merge_mark_contract(pool: &DbPool) {
    use codeyard::backend::db::{
        self,
        discussions::DiscussionEdit,
        pull_requests::records::{NewBase, NewPullRequest},
    };
    let owner = db::users::accounts::create_user(pool, &unique_name("mark-owner"), None)
        .await
        .unwrap();
    let repo_name = unique_name("mark-repo");
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{repo_name}.git"),
    )
    .await
    .unwrap();
    let sha = "a".repeat(40);
    let pull = db::pull_requests::records::create_pull_request(
        pool,
        NewPullRequest {
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: owner.id,
            title: "Merged into main",
            body: "",
            head_branch: "topic",
            head_sha: &sha,
            base_branch: "main",
            base_sha: &sha,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let merged_sha = "c".repeat(40);
    // The base moves to develop after the merge went into main, before the row is marked.
    let moved = db::pull_requests::records::update_pull_request(
        pool,
        pull.id,
        DiscussionEdit {
            actor_id: owner.id,
            expected_state: "open",
            expected_updated: pull.updated_at,
            title: None,
            body: None,
            state: None,
        },
        Some(NewBase {
            branch: "develop",
            sha: &"b".repeat(40),
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(moved.base_branch, "develop");
    assert!(
        !db::pull_requests::records::mark_pull_requests_merged(
            pool,
            std::slice::from_ref(&pull),
            owner.id,
            &merged_sha
        )
        .await
        .unwrap(),
        "the merge went into main, so the row that now targets develop is not marked"
    );
    let row = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.state.as_str(), row.base_branch.as_str(), row.merged),
        ("open", "develop", false)
    );
    assert!(
        db::pull_requests::records::mark_pull_requests_merged(
            pool,
            std::slice::from_ref(&moved),
            owner.id,
            &merged_sha
        )
        .await
        .unwrap(),
        "a row that still targets the merged branch is marked"
    );
    let row = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "merged");
}

#[tokio::test]
async fn test_pull_request_merge_mark_on_postgres() {
    let fixture = setup_test_db().await;
    pull_request_merge_mark_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_pull_request_merge_mark_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    pull_request_merge_mark_contract(&database.pool).await;
    database.close().await;
}

#[tokio::test]
async fn test_pull_request_revisions_on_postgres() {
    let fixture = setup_test_db().await;
    pull_request_revision_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_pull_request_revisions_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    pull_request_revision_contract(&database.pool).await;
    database.close().await;
}

/// The open pull requests named for a branch are those whose base branch in the repository
/// is that name, or whose head branch in the repository is. A fork's branch of the same
/// name is a different branch.
async fn pull_request_branch_filter_contract(pool: &DbPool) {
    use codeyard::backend::db::{self, pull_requests::records::NewPullRequest};
    let owner = db::users::accounts::create_user(pool, &unique_name("branch-owner"), None)
        .await
        .unwrap();
    let repo_name = unique_name("branch-repo");
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{repo_name}.git"),
    )
    .await
    .unwrap();
    let fork_name = unique_name("branch-fork");
    let fork = db::repositories::discovery::create_forked_repository(
        pool,
        db::repositories::discovery::NewForkedRepository {
            owner_id: owner.id,
            name: &fork_name,
            description: None,
            is_private: false,
            disk_path: &format!("/tmp/{fork_name}.git"),
            forked_from: repo.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    let sha = "a".repeat(40);
    let mut pulls = Vec::new();
    for (head_repo_id, head_branch, base_branch) in [
        (repo.id, "topic", "main"),
        (fork.id, "topic", "main"),
        (repo.id, "other", "release"),
    ] {
        let pull = db::pull_requests::records::create_pull_request(
            pool,
            NewPullRequest {
                repo_id: repo.id,
                head_repo_id: Some(head_repo_id),
                author_id: owner.id,
                title: head_branch,
                body: "",
                head_branch,
                head_sha: &sha,
                base_branch,
                base_sha: &sha,
                is_draft: false,
            },
        )
        .await
        .unwrap();
        pulls.push(pull.id);
    }

    assert_eq!(
        open_pull_ids_on(pool, repo.id, &["topic"]).await,
        vec![pulls[0]],
        "the fork's topic branch is not this repository's"
    );
    assert_eq!(
        open_pull_ids_on(pool, repo.id, &["main"]).await,
        vec![pulls[0], pulls[1]]
    );
    assert_eq!(
        open_pull_ids_on(pool, repo.id, &["other", "release"]).await,
        vec![pulls[2]]
    );
    assert_eq!(
        open_pull_ids_on(pool, fork.id, &["topic"]).await,
        vec![pulls[1]]
    );
    assert_eq!(
        open_pull_ids_on(pool, repo.id, &["missing"]).await,
        Vec::<i64>::new()
    );
}

async fn open_pull_ids_on(pool: &DbPool, repo_id: i64, branches: &[&str]) -> Vec<i64> {
    codeyard::backend::db::pull_requests::records::list_open_pull_request_revisions_on_branches(
        pool, repo_id, branches,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|pull| pull.id)
    .collect()
}

#[tokio::test]
async fn test_pull_request_branch_filter_on_postgres() {
    let fixture = setup_test_db().await;
    pull_request_branch_filter_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_pull_request_branch_filter_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    pull_request_branch_filter_contract(&database.pool).await;
    database.close().await;
}

/// Sets the columns that explore ranks by, the same way on both engines.
async fn set_repository_ranking(pool: &DbPool, repo_id: i64, stars: i32, created_at: &str) {
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query(
                "UPDATE repositories SET stars_count = $1, created_at = $2::timestamptz WHERE id = $3",
            )
            .bind(stars)
            .bind(created_at)
            .bind(repo_id)
            .execute(pool)
            .await
            .expect("set repository ranking");
        }
        DbPool::Sqlite(pool) => {
            sqlx::query("UPDATE repositories SET stars_count = ?1, created_at = ?2 WHERE id = ?3")
                .bind(stars)
                .bind(created_at)
                .bind(repo_id)
                .execute(pool)
                .await
                .expect("set repository ranking");
        }
    }
}

async fn ranked_repository(
    pool: &DbPool,
    owner_id: i64,
    name: &str,
    stars: i32,
    created_at: &str,
) -> i64 {
    let repo = codeyard::backend::db::repositories::records::create_repository(
        pool,
        owner_id,
        name,
        None,
        false,
        &format!("{name}.git"),
    )
    .await
    .unwrap();
    set_repository_ranking(pool, repo.id, stars, created_at).await;
    repo.id
}

/// Stars are ranked before the limit, so a bounded list keeps the most starred
/// repository even when a newer, less starred one exists.
async fn public_repository_stars_contract(pool: &DbPool) {
    use codeyard::backend::db::repositories::records::{list_public_repositories, RepositorySort};
    let owner = codeyard::backend::db::users::accounts::create_user(
        pool,
        &unique_name("stars-owner"),
        None,
    )
    .await
    .unwrap();
    let old_popular = ranked_repository(
        pool,
        owner.id,
        "old_popular",
        9,
        "2020-01-01T00:00:00.000000Z",
    )
    .await;
    let newer_tied = ranked_repository(
        pool,
        owner.id,
        "newer_tied",
        9,
        "2021-01-01T00:00:00.000000Z",
    )
    .await;
    let middling =
        ranked_repository(pool, owner.id, "middling", 3, "2022-01-01T00:00:00.000000Z").await;
    let newest =
        ranked_repository(pool, owner.id, "newest", 0, "2023-01-01T00:00:00.000000Z").await;

    let top = list_public_repositories(pool, RepositorySort::Stars, 1)
        .await
        .unwrap();
    assert_eq!(
        top.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        [newer_tied]
    );
    let ranked = list_public_repositories(pool, RepositorySort::Stars, 4)
        .await
        .unwrap();
    assert_eq!(
        ranked.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        [newer_tied, old_popular, middling, newest]
    );
    let recent = list_public_repositories(pool, RepositorySort::Recent, 1)
        .await
        .unwrap();
    assert_eq!(
        recent.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        [newest]
    );
}

#[tokio::test]
async fn test_public_repository_stars_order_on_postgres() {
    let fixture = setup_test_db().await;
    public_repository_stars_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_public_repository_stars_order_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    public_repository_stars_contract(&database.pool).await;
    database.close().await;
}

/// Search matches a public repository's name or description, ignoring case, and never a private
/// one. A shared repository is one that another user gives the viewer access to.
async fn public_repository_search_contract(pool: &DbPool) {
    use codeyard::backend::db::models::AccessLevel;
    use codeyard::backend::db::repositories::{
        access::grant_repo_access,
        records::{
            list_shared_repositories, search_public_repositories, RepositoryFilter, RepositorySort,
        },
    };
    let owner = codeyard::backend::db::users::accounts::create_user(
        pool,
        &unique_name("search-owner"),
        None,
    )
    .await
    .unwrap();
    let viewer = codeyard::backend::db::users::accounts::create_user(
        pool,
        &unique_name("search-viewer"),
        None,
    )
    .await
    .unwrap();
    let token = unique_name("tokenname").replace('-', "");
    let by_name = codeyard::backend::db::repositories::records::create_repository(
        pool,
        owner.id,
        &format!("{token}-repo"),
        None,
        false,
        &format!("{token}-repo.git"),
    )
    .await
    .unwrap();
    let by_description = codeyard::backend::db::repositories::records::create_repository(
        pool,
        owner.id,
        &unique_name("plain"),
        Some("A Rust Tooling kit"),
        false,
        &format!("{token}-plain.git"),
    )
    .await
    .unwrap();
    let secret = codeyard::backend::db::repositories::records::create_repository(
        pool,
        owner.id,
        &unique_name("secret"),
        Some("Rust Tooling in private"),
        true,
        &format!("{token}-secret.git"),
    )
    .await
    .unwrap();

    let named = search_public_repositories(
        pool,
        &token,
        RepositoryFilter::default(),
        RepositorySort::Recent,
        10,
        0,
    )
    .await
    .unwrap();
    assert_eq!(
        named.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        [by_name.id]
    );
    let described = search_public_repositories(
        pool,
        "rust TOOLING",
        RepositoryFilter::default(),
        RepositorySort::Stars,
        100,
        0,
    )
    .await
    .unwrap();
    assert!(described.iter().any(|repo| repo.id == by_description.id));
    assert!(!described.iter().any(|repo| repo.id == secret.id));
    let everything = search_public_repositories(
        pool,
        "",
        RepositoryFilter::default(),
        RepositorySort::Recent,
        100,
        0,
    )
    .await
    .unwrap();
    assert!(everything.iter().any(|repo| repo.id == by_name.id));
    assert!(!everything.iter().any(|repo| repo.id == secret.id));
    // A later page starts where the earlier one ends, with no repeats.
    let first_two = search_public_repositories(
        pool,
        "",
        RepositoryFilter::default(),
        RepositorySort::Recent,
        2,
        0,
    )
    .await
    .unwrap();
    let second = search_public_repositories(
        pool,
        "",
        RepositoryFilter::default(),
        RepositorySort::Recent,
        1,
        1,
    )
    .await
    .unwrap();
    assert_eq!(first_two.len(), 2);
    assert_eq!(first_two[0].id, everything[0].id);
    assert_eq!(
        second.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        [first_two[1].id]
    );

    assert!(list_shared_repositories(pool, viewer.id)
        .await
        .unwrap()
        .is_empty());
    grant_repo_access(pool, viewer.id, by_name.id, AccessLevel::Write)
        .await
        .unwrap();
    let shared = list_shared_repositories(pool, viewer.id).await.unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].repository.id, by_name.id);
    assert_eq!(shared[0].access_level, AccessLevel::Write as i16);
    assert!(list_shared_repositories(pool, owner.id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_public_repository_search_and_shares_on_postgres() {
    let fixture = setup_test_db().await;
    public_repository_search_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_public_repository_search_and_shares_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    public_repository_search_contract(&database.pool).await;
    database.close().await;
}

/// Sets a row's state directly. The contract needs closed and merged rows without going through
/// the workflows that set them.
async fn set_state(pool: &DbPool, table: &str, id: i64, state: &str) {
    match pool {
        DbPool::Sqlite(pool) => {
            sqlx::query(&format!("UPDATE {table} SET state = ?1 WHERE id = ?2"))
                .bind(state)
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }
        DbPool::Postgres(pool) => {
            sqlx::query(&format!("UPDATE {table} SET state = $1 WHERE id = $2"))
                .bind(state)
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

fn sorted(mut titles: Vec<&str>) -> Vec<&str> {
    titles.sort_unstable();
    titles
}

/// A viewer's lists hold the issues and pull requests assigned to them, opened by them, or waiting
/// for their review, in repositories they can read. Closed and merged items are in the closed list.
async fn viewer_work_contract(pool: &DbPool) {
    use codeyard::backend::db::{
        self,
        assignees::{assign_user, AssignedItem},
        issues::IssueRelation,
        models::AccessLevel,
        pull_requests::records::{NewPullRequest, PullRequestRelation},
    };
    let owner = db::users::accounts::create_user(pool, &unique_name("work-owner"), None)
        .await
        .unwrap();
    let viewer = db::users::accounts::create_user(pool, &unique_name("work-viewer"), None)
        .await
        .unwrap();
    let public_name = unique_name("work-public");
    let public = db::repositories::records::create_repository(
        pool,
        owner.id,
        &public_name,
        None,
        false,
        &format!("/tmp/{public_name}.git"),
    )
    .await
    .unwrap();
    // Only the owner and collaborators can be assigned, so the viewer collaborates here first.
    db::repositories::access::grant_repo_access(pool, viewer.id, public.id, AccessLevel::Write)
        .await
        .unwrap();
    let private_name = unique_name("work-private");
    let private = db::repositories::records::create_repository(
        pool,
        owner.id,
        &private_name,
        None,
        true,
        &format!("/tmp/{private_name}.git"),
    )
    .await
    .unwrap();

    let assigned = db::issues::create_issue(pool, public.id, owner.id, "assigned open", "")
        .await
        .unwrap();
    assign_user(
        pool,
        owner.id,
        public.id,
        AssignedItem::Issue(assigned.id),
        viewer.id,
    )
    .await
    .unwrap();
    let assigned_closed =
        db::issues::create_issue(pool, public.id, owner.id, "assigned closed", "")
            .await
            .unwrap();
    assign_user(
        pool,
        owner.id,
        public.id,
        AssignedItem::Issue(assigned_closed.id),
        viewer.id,
    )
    .await
    .unwrap();
    set_state(pool, "issues", assigned_closed.id, "closed").await;
    db::issues::create_issue(pool, public.id, viewer.id, "opened open", "")
        .await
        .unwrap();
    let opened_closed = db::issues::create_issue(pool, public.id, viewer.id, "opened closed", "")
        .await
        .unwrap();
    set_state(pool, "issues", opened_closed.id, "closed").await;
    db::issues::create_issue(pool, public.id, owner.id, "unrelated", "")
        .await
        .unwrap();
    // Assigned in a private repository while the viewer could read it; hidden once access ends.
    let hidden = db::issues::create_issue(pool, private.id, owner.id, "private assigned", "")
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(pool, viewer.id, private.id, AccessLevel::Write)
        .await
        .unwrap();
    assign_user(
        pool,
        owner.id,
        private.id,
        AssignedItem::Issue(hidden.id),
        viewer.id,
    )
    .await
    .unwrap();
    db::repositories::access::revoke_repo_access(pool, viewer.id, private.id)
        .await
        .unwrap();

    let assigned_open =
        db::issues::list_viewer_issues(pool, viewer.id, IssueRelation::Assigned, true, 100)
            .await
            .unwrap();
    assert_eq!(
        sorted(
            assigned_open
                .iter()
                .map(|e| e.issue.title.as_str())
                .collect()
        ),
        ["assigned open"]
    );
    let created_closed =
        db::issues::list_viewer_issues(pool, viewer.id, IssueRelation::Created, false, 100)
            .await
            .unwrap();
    assert_eq!(
        sorted(
            created_closed
                .iter()
                .map(|e| e.issue.title.as_str())
                .collect()
        ),
        ["opened closed"]
    );
    assert_eq!(created_closed[0].repo_owner, owner.username);
    let counts = db::issues::count_viewer_issues(pool, viewer.id, IssueRelation::Assigned)
        .await
        .unwrap();
    assert_eq!((counts.open, counts.closed), (1, 1));
    let counts = db::issues::count_viewer_issues(pool, viewer.id, IssueRelation::Created)
        .await
        .unwrap();
    assert_eq!((counts.open, counts.closed), (1, 1));

    db::repositories::access::grant_repo_access(pool, viewer.id, private.id, AccessLevel::Write)
        .await
        .unwrap();
    let assigned_open =
        db::issues::list_viewer_issues(pool, viewer.id, IssueRelation::Assigned, true, 100)
            .await
            .unwrap();
    assert_eq!(
        sorted(
            assigned_open
                .iter()
                .map(|e| e.issue.title.as_str())
                .collect()
        ),
        ["assigned open", "private assigned"]
    );

    // Pull requests: opened, merged (closed), assigned, and a review requested in a private
    // repository that the viewer can read only while granted.
    const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
    const BASE_SHA: &str = "2222222222222222222222222222222222222222";
    let new_pr =
        |repo_id: i64, author_id: i64, title: &'static str, branch: &'static str| NewPullRequest {
            head_repo_id: None,
            repo_id,
            author_id,
            title,
            body: "",
            head_branch: branch,
            head_sha: HEAD_SHA,
            base_branch: "main",
            base_sha: BASE_SHA,
            is_draft: false,
        };
    db::pull_requests::records::create_pull_request(
        pool,
        new_pr(public.id, viewer.id, "opened pr", "opened"),
    )
    .await
    .unwrap();
    let merged = db::pull_requests::records::create_pull_request(
        pool,
        new_pr(public.id, viewer.id, "merged pr", "merged"),
    )
    .await
    .unwrap();
    set_state(pool, "pull_requests", merged.id, "merged").await;
    let assigned_pr = db::pull_requests::records::create_pull_request(
        pool,
        new_pr(public.id, owner.id, "assigned pr", "assigned"),
    )
    .await
    .unwrap();
    assign_user(
        pool,
        owner.id,
        public.id,
        AssignedItem::PullRequest(assigned_pr.id),
        viewer.id,
    )
    .await
    .unwrap();
    let review_pr = db::pull_requests::records::create_pull_request(
        pool,
        new_pr(public.id, owner.id, "review pr", "review"),
    )
    .await
    .unwrap();
    assert!(db::pull_requests::request_pull_request_reviewer(
        pool,
        review_pr.id,
        owner.id,
        viewer.id
    )
    .await
    .unwrap());
    let private_pr = db::pull_requests::records::create_pull_request(
        pool,
        new_pr(private.id, owner.id, "private review pr", "private-review"),
    )
    .await
    .unwrap();
    assert!(db::pull_requests::request_pull_request_reviewer(
        pool,
        private_pr.id,
        owner.id,
        viewer.id
    )
    .await
    .unwrap());
    db::repositories::access::revoke_repo_access(pool, viewer.id, private.id)
        .await
        .unwrap();
    let review_open = db::pull_requests::records::list_viewer_pull_requests(
        pool,
        viewer.id,
        PullRequestRelation::ReviewRequested,
        true,
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        sorted(
            review_open
                .iter()
                .map(|e| e.pull_request.title.as_str())
                .collect()
        ),
        ["review pr"]
    );
    db::repositories::access::grant_repo_access(pool, viewer.id, private.id, AccessLevel::Read)
        .await
        .unwrap();
    let review_open = db::pull_requests::records::list_viewer_pull_requests(
        pool,
        viewer.id,
        PullRequestRelation::ReviewRequested,
        true,
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        sorted(
            review_open
                .iter()
                .map(|e| e.pull_request.title.as_str())
                .collect()
        ),
        ["private review pr", "review pr"]
    );
    let assigned_pr_open = db::pull_requests::records::list_viewer_pull_requests(
        pool,
        viewer.id,
        PullRequestRelation::Assigned,
        true,
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        sorted(
            assigned_pr_open
                .iter()
                .map(|e| e.pull_request.title.as_str())
                .collect()
        ),
        ["assigned pr"]
    );
    let created_closed = db::pull_requests::records::list_viewer_pull_requests(
        pool,
        viewer.id,
        PullRequestRelation::Created,
        false,
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        sorted(
            created_closed
                .iter()
                .map(|e| e.pull_request.title.as_str())
                .collect()
        ),
        ["merged pr"]
    );
    let counts = db::pull_requests::records::count_viewer_pull_requests(
        pool,
        viewer.id,
        PullRequestRelation::Created,
    )
    .await
    .unwrap();
    assert_eq!((counts.open, counts.closed), (1, 1));
    // Involved is the three relations together, each pull request once.
    let involved_open = db::pull_requests::records::list_viewer_pull_requests(
        pool,
        viewer.id,
        PullRequestRelation::Involved,
        true,
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        sorted(
            involved_open
                .iter()
                .map(|e| e.pull_request.title.as_str())
                .collect()
        ),
        ["assigned pr", "opened pr", "private review pr", "review pr"]
    );
    let involved = db::pull_requests::records::count_viewer_pull_requests(
        pool,
        viewer.id,
        PullRequestRelation::Involved,
    )
    .await
    .unwrap();
    assert_eq!((involved.open, involved.closed), (4, 1));
}

/// A label's count is the issues and pull requests it is on, and a label on neither counts 0.
async fn label_usage_contract(pool: &DbPool) {
    use codeyard::backend::db::{self, pull_requests::records::NewPullRequest};
    let owner = db::users::accounts::create_user(pool, &unique_name("usage-owner"), None)
        .await
        .unwrap();
    let repo_name = unique_name("usage-repo");
    let repo = db::repositories::records::create_repository(
        pool,
        owner.id,
        &repo_name,
        None,
        false,
        &format!("/tmp/{repo_name}.git"),
    )
    .await
    .unwrap();
    let bug = db::labels::create_label(pool, owner.id, repo.id, "bug", "#d73a4a", None)
        .await
        .unwrap();
    let docs = db::labels::create_label(pool, owner.id, repo.id, "docs", "#0075ca", None)
        .await
        .unwrap();
    let unused = db::labels::create_label(pool, owner.id, repo.id, "unused", "#ededed", None)
        .await
        .unwrap();
    let issue = db::issues::create_issue(pool, repo.id, owner.id, "usage issue", "")
        .await
        .unwrap();
    db::labels::add_issue_labels(pool, owner.id, repo.id, issue.id, &[bug.id, docs.id])
        .await
        .unwrap();
    let pr = db::pull_requests::records::create_pull_request(
        pool,
        NewPullRequest {
            head_repo_id: None,
            repo_id: repo.id,
            author_id: owner.id,
            title: "usage pr",
            body: "",
            head_branch: "usage",
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "2222222222222222222222222222222222222222",
            is_draft: false,
        },
    )
    .await
    .unwrap();
    db::labels::add_pull_request_labels(pool, owner.id, repo.id, pr.id, &[bug.id])
        .await
        .unwrap();
    let usage: std::collections::BTreeMap<i64, i64> = db::labels::list_label_usage(pool, repo.id)
        .await
        .unwrap()
        .into_iter()
        .collect();
    let expected: std::collections::BTreeMap<i64, i64> =
        [(bug.id, 2), (docs.id, 1), (unused.id, 0)]
            .into_iter()
            .collect();
    assert_eq!(usage, expected);

    // Another repository's labels and items do not count here, and are counted there.
    let other = db::repositories::records::create_repository(
        pool,
        owner.id,
        &format!("{repo_name}-other"),
        None,
        false,
        &format!("/tmp/{repo_name}-other.git"),
    )
    .await
    .unwrap();
    let other_bug =
        db::labels::create_label(pool, owner.id, other.id, "other-bug", "#d73a4a", None)
            .await
            .unwrap();
    let other_issue = db::issues::create_issue(pool, other.id, owner.id, "other issue", "")
        .await
        .unwrap();
    db::labels::add_issue_labels(pool, owner.id, other.id, other_issue.id, &[other_bug.id])
        .await
        .unwrap();
    let usage: std::collections::BTreeMap<i64, i64> = db::labels::list_label_usage(pool, repo.id)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(usage, expected);
    let other_usage: Vec<(i64, i64)> = db::labels::list_label_usage(pool, other.id).await.unwrap();
    assert_eq!(other_usage, vec![(other_bug.id, 1)]);
}

#[tokio::test]
async fn test_label_usage_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    label_usage_contract(&database.pool).await;
    database.close().await;
}

#[tokio::test]
async fn test_label_usage_on_postgres() {
    let fixture = setup_test_db().await;
    label_usage_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_viewer_work_lists_on_postgres() {
    let fixture = setup_test_db().await;
    viewer_work_contract(&fixture.pool).await;
}

#[tokio::test]
async fn test_viewer_work_lists_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    viewer_work_contract(&database.pool).await;
    database.close().await;
}
