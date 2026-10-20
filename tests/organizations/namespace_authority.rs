//! Namespace contracts on disposable PostgreSQL schemas and SQLite files: users
//! and organizations cannot share a name. A missing PostgreSQL URL fails the
//! test; nothing is skipped. No provider or process IO.
#![cfg(not(target_arch = "wasm32"))]

use crate::support::production_ci;

use codeyard::backend::db::{
    self,
    organizations::{
        create_organization, NewOrganization, OrganizationMetadata, OrganizationOutcome,
        OrganizationProfile,
    },
    users::{
        signup_requests::ReviewError,
        transitions::{self, AccountAdmission, SetupClaim},
    },
};
use futures_util::FutureExt;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::{panic::AssertUnwindSafe, str::FromStr, time::Duration};

// Raw SQL below only prepares or observes owned fixtures.
enum Raw {
    Postgres(sqlx::PgPool),
    Sqlite(sqlx::SqlitePool),
}
struct Database {
    pool: db::DbPool,
    raw: Raw,
    pg: Option<production_ci::Fixture>,
    _directory: Option<tempfile::TempDir>,
}
impl Database {
    async fn postgres() -> Self {
        let pg = production_ci::Fixture::new().await;
        // This support fixture creates one CI repository/account that this suite
        // does not use. Remove ONLY those freshly generated scaffolding IDs before
        // beginning the zero-account setup tests, never any other accounts.
        sqlx::query("DELETE FROM repositories WHERE id=$1")
            .bind(pg.repo.id)
            .execute(&pg.raw)
            .await
            .expect("remove owned generated CI repository");
        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(pg.owner.id)
            .execute(&pg.raw)
            .await
            .expect("remove owned generated CI account");
        Self {
            pool: pg.pool.clone(),
            raw: Raw::Postgres(pg.raw.clone()),
            pg: Some(pg),
            _directory: None,
        }
    }

    async fn sqlite() -> Self {
        let directory = tempfile::tempdir().expect("owned namespace SQLite directory");
        let url = format!(
            "sqlite://{}",
            directory.path().join("namespace.sqlite").display()
        );
        let raw = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(
                SqliteConnectOptions::from_str(&url)
                    .expect("owned SQLite URL")
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .busy_timeout(Duration::from_secs(5)),
            )
            .await
            .expect("required SQLite fixture");
        let pool = raw.clone().into();
        db::run_migrations(&pool)
            .await
            .expect("required native SQLite migrations");
        Self {
            pool,
            raw: Raw::Sqlite(raw),
            pg: None,
            _directory: Some(directory),
        }
    }

    async fn execute(&self, sql: &str) {
        match &self.raw {
            Raw::Postgres(pool) => {
                sqlx::query(sql)
                    .execute(pool)
                    .await
                    .expect("owned PG mutation");
            }
            Raw::Sqlite(pool) => {
                sqlx::query(sql)
                    .execute(pool)
                    .await
                    .expect("owned SQLite mutation");
            }
        }
    }

    async fn count(&self, sql: &str, name: &str) -> i64 {
        match &self.raw {
            Raw::Postgres(pool) => sqlx::query_scalar::<_, i64>(sql)
                .bind(name)
                .fetch_one(pool)
                .await
                .expect("owned PG observation"),
            Raw::Sqlite(pool) => sqlx::query_scalar::<_, i64>(sql)
                .bind(name)
                .fetch_one(pool)
                .await
                .expect("owned SQLite observation"),
        }
    }

    async fn org(&self, actor: i64, name: &str) -> OrganizationMetadata {
        match create_organization(&self.pool, actor, &organization(name))
            .await
            .unwrap()
        {
            OrganizationOutcome::Applied(org) => org,
            _ => panic!("expected admitted organization"),
        }
    }

    async fn close(&mut self) {
        // All joins below are awaited directly on this original runtime. No
        // spawned/detached task, timed-out JoinHandle or active borrower remains.
        self.pool.close().await;
        match &self.raw {
            Raw::Postgres(pool) => pool.close().await,
            Raw::Sqlite(pool) => pool.close().await,
        }
        if let Some(pg) = &mut self.pg {
            pg.finish().await;
        }
    }
}

fn organization(name: &str) -> NewOrganization {
    NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap()
}
fn admitted(outcome: AccountAdmission) -> db::models::User {
    match outcome {
        AccountAdmission::User(user) => *user,
        _ => panic!("expected admitted account"),
    }
}
fn namespace_refusal(result: anyhow::Result<db::models::User>) {
    let error = result.expect_err("organization name must not admit a user");
    assert_eq!(error.to_string(), "Account namespace conflict");
    assert_eq!(format!("{error:#}"), "Account namespace conflict");
}
fn review_denied(result: anyhow::Result<db::models::User>) {
    let error = result.expect_err("approval must be denied");
    assert!(matches!(
        error.downcast_ref::<ReviewError>(),
        Some(ReviewError::Denied)
    ));
}

async fn setup_namespace_and_exact_recovery(database: &Database) -> db::models::User {
    assert_eq!(
        db::users::accounts::count_users(&database.pool)
            .await
            .unwrap(),
        0
    );
    let admin = match transitions::complete_setup(
        &database.pool,
        "recovery_admin",
        None,
        "original-hash",
        "public",
        None,
    )
    .await
    .unwrap()
    {
        SetupClaim::Completed(user) => *user,
        _ => panic!("free namespace must permit new-account setup"),
    };
    assert!(admin.is_admin);
    // Reopen setup while the sole admin account already exists. Exact recovery
    // must remain usable.
    database
        .execute("UPDATE node_configuration SET setup_completed=FALSE WHERE id=1")
        .await;
    for (name, expected) in [
        ("outsider", "original-hash"),
        ("recovery_admin", "stale-hash"),
    ] {
        assert!(matches!(
            transitions::complete_setup(
                &database.pool,
                name,
                None,
                "replacement",
                "public",
                Some((admin.id, expected))
            )
            .await
            .unwrap(),
            SetupClaim::ExistingAccounts
        ));
    }
    let recovered = match transitions::complete_setup(
        &database.pool,
        "recovery_admin",
        None,
        "replacement",
        "public",
        Some((admin.id, "original-hash")),
    )
    .await
    .unwrap()
    {
        SetupClaim::Completed(user) => *user,
        _ => panic!("exact setup recovery must remain usable"),
    };
    assert_eq!(recovered.id, admin.id);
    assert_eq!(recovered.password_hash.as_deref(), Some("replacement"));
    assert!(matches!(
        transitions::complete_setup(&database.pool, "another", None, "hash", "public", None)
            .await
            .unwrap(),
        SetupClaim::AlreadyCompleted
    ));
    assert_eq!(
        db::users::accounts::count_users(&database.pool)
            .await
            .unwrap(),
        1
    );
    recovered
}

async fn primitive_paths_and_case_policy(database: &Database, actor: i64) {
    database.org(actor, "primitive_occupied").await;
    namespace_refusal(
        db::users::accounts::create_user(&database.pool, "primitive_occupied", None).await,
    );
    namespace_refusal(
        db::users::accounts::create_user_with_password(
            &database.pool,
            "primitive_occupied",
            None,
            "hash",
        )
        .await,
    );
    namespace_refusal(
        db::users::accounts::create_user_with_password_and_flags(
            &database.pool,
            "primitive_occupied",
            None,
            "hash",
            true,
            true,
        )
        .await,
    );
    namespace_refusal(
        db::users::accounts::create_user_from_oauth(
            &database.pool,
            "primitive_occupied",
            Some("secret@example.test"),
            Some("private profile"),
        )
        .await,
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM users WHERE username=$1",
                "primitive_occupied"
            )
            .await,
        0
    );

    // The other sequential direction: user exists before organization admission.
    db::users::accounts::create_user(&database.pool, "user_first", None)
        .await
        .unwrap();
    assert!(matches!(
        create_organization(&database.pool, actor, &organization("user_first"))
            .await
            .unwrap(),
        OrganizationOutcome::Conflict
    ));
    // Keep exact policy in BOTH directions; no lowercasing/renaming.
    database.org(actor, "User_first").await;
    db::users::accounts::create_user(&database.pool, "Primitive_occupied", None)
        .await
        .unwrap();
    let password = db::users::accounts::create_user_with_password(
        &database.pool,
        "password_free",
        None,
        "hash",
    )
    .await
    .unwrap();
    assert_eq!(password.password_hash.as_deref(), Some("hash"));
    let flags = db::users::accounts::create_user_with_password_and_flags(
        &database.pool,
        "flags_free",
        None,
        "hash",
        true,
        true,
    )
    .await
    .unwrap();
    assert!(flags.is_admin && flags.must_change_password);
    let oauth = db::users::accounts::create_user_from_oauth(
        &database.pool,
        "direct_oauth_free",
        Some("direct@example.test"),
        None,
    )
    .await
    .unwrap();
    assert!(oauth.email_verified);
    assert!(matches!(
        create_organization(&database.pool, actor, &organization("direct_oauth_free"))
            .await
            .unwrap(),
        OrganizationOutcome::Conflict
    ));
}

async fn account_admission_and_existing_identity(database: &Database, actor: i64) {
    database.org(actor, "password_occupied").await;
    assert!(matches!(
        transitions::register_password(&database.pool, "password_occupied", None, "hash", None)
            .await
            .unwrap(),
        AccountAdmission::Conflict
    ));
    let public = admitted(
        transitions::register_password(&database.pool, "public_free", None, "hash", None)
            .await
            .unwrap(),
    );
    assert!(!public.is_admin && !public.must_change_password);
    assert!(matches!(
        transitions::register_password(&database.pool, "public_free", None, "hash", None)
            .await
            .unwrap(),
        AccountAdmission::Conflict
    ));

    database.org(actor, "oauth_base").await;
    db::users::accounts::create_user(&database.pool, "oauth_base_1", None)
        .await
        .unwrap();
    database.org(actor, "oauth_base_2").await;
    let oauth = admitted(
        transitions::admit_oauth_account(
            &database.pool,
            "github",
            "suffix-subject",
            "oauth_base",
            None,
        )
        .await
        .unwrap(),
    );
    assert_eq!(oauth.username, "oauth_base_3");
    let again = admitted(
        transitions::admit_oauth_account(
            &database.pool,
            "github",
            "suffix-subject",
            "ignored_retry",
            None,
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        again.id, oauth.id,
        "existing subject login must retain its identity"
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM oauth_accounts WHERE provider_user_id=$1",
                "suffix-subject"
            )
            .await,
        1
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM users WHERE username=$1",
                "ignored_retry"
            )
            .await,
        0
    );

    // Existing verified-email account resolution is unchanged: a lookup never becomes
    // a new identity. The current OAuth operation uses verified subjects, NOT email
    // linking.
    let verified = db::users::accounts::create_user_from_oauth(
        &database.pool,
        "verified_user",
        Some("verified@example.test"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        db::users::accounts::get_user_by_email(&database.pool, "verified@example.test")
            .await
            .unwrap()
            .unwrap()
            .id,
        verified.id
    );
    assert_eq!(
        db::users::accounts::get_user_by_username(&database.pool, "verified_user")
            .await
            .unwrap()
            .unwrap()
            .id,
        verified.id
    );
    database
        .execute("UPDATE node_configuration SET node_mode='private' WHERE id=1")
        .await;
    assert!(transitions::enroll_oauth_identity(
        &database.pool,
        actor,
        verified.id,
        "github",
        "verified-subject"
    )
    .await
    .unwrap());
    let login = admitted(
        transitions::admit_oauth_account(
            &database.pool,
            "github",
            "verified-subject",
            "verified_user",
            None,
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        login.id, verified.id,
        "verified-email account with approved subject remains the same account"
    );
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM users WHERE username=$1",
                "verified_user"
            )
            .await,
        1
    );
    database
        .execute("UPDATE node_configuration SET node_mode='public' WHERE id=1")
        .await;

    // Bounded suffix exhaustion returns a real Conflict with no orphan identity.
    for suffix in 0..32 {
        let name = if suffix == 0 {
            "exhausted".to_owned()
        } else {
            format!("exhausted_{suffix}")
        };
        database.org(actor, &name).await;
    }
    assert!(matches!(
        transitions::admit_oauth_account(
            &database.pool,
            "github",
            "exhausted-subject",
            "exhausted",
            None
        )
        .await
        .unwrap(),
        AccountAdmission::Conflict
    ));
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM oauth_accounts WHERE provider_user_id=$1",
                "exhausted-subject"
            )
            .await,
        0
    );
}

async fn signup_review_atomicity(database: &Database, admin: i64) {
    database
        .execute("UPDATE node_configuration SET node_mode='private' WHERE id=1")
        .await;
    database.org(admin, "private_occupied").await;
    db::users::sessions::create_session(
        &database.pool,
        "namespace-admin",
        admin,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    for administrator in [None, Some((admin, "namespace-admin", false))] {
        assert!(matches!(
            transitions::register_password(
                &database.pool,
                "private_occupied",
                None,
                "hash",
                administrator
            )
            .await
            .unwrap(),
            AccountAdmission::Conflict
        ));
    }
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM signup_requests WHERE username=$1",
                "private_occupied"
            )
            .await,
        0
    );
    let created = admitted(
        transitions::register_password(
            &database.pool,
            "admin_created",
            None,
            "hash",
            Some((admin, "namespace-admin", false)),
        )
        .await
        .unwrap(),
    );
    assert!(created.must_change_password && !created.is_admin);

    assert!(matches!(
        transitions::register_password(&database.pool, "pending_collision", None, "hash", None)
            .await
            .unwrap(),
        AccountAdmission::Requested
    ));
    assert!(matches!(
        transitions::register_password(&database.pool, "PENDING_COLLISION", None, "hash", None)
            .await
            .unwrap(),
        AccountAdmission::Conflict
    ));
    let request = db::users::signup_requests::get_pending_signup_request_by_identity(
        &database.pool,
        "pending_collision",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    database.org(admin, "pending_collision").await;
    review_denied(
        db::users::signup_requests::approve_signup_request(&database.pool, request.id, admin).await,
    );
    let pending = db::users::signup_requests::get_pending_signup_request_by_identity(
        &database.pool,
        "pending_collision",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(pending.id, request.id);
    assert!(pending.reviewed_by.is_none() && pending.reviewed_at.is_none());
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM users WHERE username=$1",
                "pending_collision"
            )
            .await,
        0
    );

    // Direct request creation is not namespace reservation. Approval rechecks it.
    let direct = db::users::signup_requests::create_signup_request(
        &database.pool,
        "private_occupied",
        None,
        "hash",
    )
    .await
    .unwrap();
    review_denied(
        db::users::signup_requests::approve_signup_request(&database.pool, direct.id, admin).await,
    );
    let outsider = db::users::accounts::create_user(&database.pool, "review_outsider", None)
        .await
        .unwrap();
    let valid = db::users::signup_requests::create_signup_request(
        &database.pool,
        "approval_free",
        None,
        "hash",
    )
    .await
    .unwrap();
    review_denied(
        db::users::signup_requests::approve_signup_request(&database.pool, valid.id, outsider.id)
            .await,
    );
    for flag in ["is_banned", "must_change_password"] {
        database
            .execute(&format!(
                "UPDATE users SET {flag}=TRUE WHERE username='recovery_admin'"
            ))
            .await;
        review_denied(
            db::users::signup_requests::approve_signup_request(&database.pool, valid.id, admin)
                .await,
        );
        assert!(
            db::users::signup_requests::get_pending_signup_request_by_identity(
                &database.pool,
                "approval_free",
                None
            )
            .await
            .unwrap()
            .is_some()
        );
        database
            .execute(&format!(
                "UPDATE users SET {flag}=FALSE WHERE username='recovery_admin'"
            ))
            .await;
    }
    let (a, b) = tokio::join!(
        db::users::signup_requests::approve_signup_request(&database.pool, valid.id, admin),
        db::users::signup_requests::approve_signup_request(&database.pool, valid.id, admin),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let error = match (a, b) {
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => error,
        _ => panic!("exactly one approval must commit"),
    };
    assert!(matches!(
        error.downcast_ref::<ReviewError>(),
        Some(ReviewError::Processed)
    ));
    assert_eq!(
        database
            .count(
                "SELECT COUNT(*) FROM users WHERE username=$1",
                "approval_free"
            )
            .await,
        1
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM signup_requests WHERE username=$1 AND status='approved' AND reviewed_by IS NOT NULL AND reviewed_at IS NOT NULL", "approval_free").await, 1);
    database
        .execute("UPDATE node_configuration SET node_mode='public' WHERE id=1")
        .await;
}

async fn concurrent_namespace_winners(database: &Database, actor: i64) {
    for index in 0..8 {
        let name = format!("namespace_race_{index}");
        let input = organization(&name);
        // Directly joined futures, never spawned tasks/timeout-detached handles.
        let (user, org) = tokio::join!(
            db::users::accounts::create_user(&database.pool, &name, None),
            create_organization(&database.pool, actor, &input),
        );
        let org = org.unwrap();
        assert_eq!(
            usize::from(user.is_ok()) + usize::from(matches!(org, OrganizationOutcome::Applied(_))),
            1
        );
        match org {
            OrganizationOutcome::Applied(_) => namespace_refusal(user),
            OrganizationOutcome::Conflict => {
                user.expect("user wins reciprocal conflict");
            }
            _ => panic!("active creator cannot be denied or missing"),
        }
        let users = database
            .count("SELECT COUNT(*) FROM users WHERE username=$1", &name)
            .await;
        let orgs = database
            .count("SELECT COUNT(*) FROM organizations WHERE name=$1", &name)
            .await;
        assert_eq!(users + orgs, 1);
        assert_eq!(database.count("SELECT COUNT(*) FROM org_memberships m JOIN organizations o ON o.id=m.org_id WHERE o.name=$1 AND m.role='owner'", &name).await, orgs, "winner org has exactly one owner; losing org leaves no orphan membership");
    }
    let input = organization("separate_org");
    let (user, org) = tokio::join!(
        db::users::accounts::create_user(&database.pool, "separate_user", None),
        create_organization(&database.pool, actor, &input),
    );
    user.expect("separate user namespace succeeds");
    assert!(matches!(org.unwrap(), OrganizationOutcome::Applied(_)));
}

async fn wait_for_pg_blocked(raw: &sqlx::PgPool, holder: i32) -> i32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: Option<i32> = sqlx::query_scalar(
                "SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) LIMIT 1",
            )
            .bind(holder)
            .fetch_optional(raw)
            .await
            .unwrap();
            if let Some(pid) = waiting {
                return pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("registration must reach the database lock")
}

async fn managed_account_sessions(database: &Database) {
    use db::users::{accounts, sessions};

    let SetupClaim::Completed(admin) =
        transitions::complete_setup(&database.pool, "admin", None, "old-hash", "private", None)
            .await
            .unwrap()
    else {
        panic!("private setup must complete");
    };
    let other = accounts::create_user_with_password_and_flags(
        &database.pool,
        "other-admin",
        None,
        "hash",
        true,
        false,
    )
    .await
    .unwrap();
    for (id, actor, expires) in [
        (
            "original",
            admin.id,
            chrono::Utc::now() + chrono::Duration::hours(1),
        ),
        (
            "replacement",
            admin.id,
            chrono::Utc::now() + chrono::Duration::hours(1),
        ),
        (
            "foreign",
            other.id,
            chrono::Utc::now() + chrono::Duration::hours(1),
        ),
        (
            "expired",
            admin.id,
            chrono::Utc::now() - chrono::Duration::seconds(1),
        ),
    ] {
        sessions::create_session(&database.pool, id, actor, None, None, expires)
            .await
            .unwrap();
    }
    let created = admitted(
        transitions::register_password(
            &database.pool,
            "before_logout",
            None,
            "hash",
            Some((admin.id, "original", false)),
        )
        .await
        .unwrap(),
    );
    assert!(created.must_change_password && !created.is_admin);
    sessions::delete_session(&database.pool, "original")
        .await
        .unwrap();
    for session in ["missing", "foreign", "expired", "original"] {
        assert!(matches!(
            transitions::register_password(
                &database.pool,
                "denied",
                None,
                "hash",
                Some((admin.id, session, false)),
            )
            .await
            .unwrap(),
            AccountAdmission::Denied
        ));
    }
    assert!(accounts::get_user_by_username(&database.pool, "denied")
        .await
        .unwrap()
        .is_none());
    // Another live session for the same admin must not rescue the revoked one.
    let created = admitted(
        transitions::register_password(
            &database.pool,
            "before_password_change",
            None,
            "hash",
            Some((admin.id, "replacement", false)),
        )
        .await
        .unwrap(),
    );
    assert!(transitions::compare_exchange_password(
        &database.pool,
        admin.id,
        "old-hash",
        "new-hash",
        false,
        true,
        None,
    )
    .await
    .unwrap()
    .is_some());
    assert!(accounts::get_user_by_id(&database.pool, created.id)
        .await
        .unwrap()
        .is_some());
    assert!(matches!(
        transitions::register_password(
            &database.pool,
            "after_password_change",
            None,
            "hash",
            Some((admin.id, "replacement", false)),
        )
        .await
        .unwrap(),
        AccountAdmission::Denied
    ));

    for change in ["password", "logout", "expiry"] {
        let expires = chrono::Utc::now()
            + if change == "expiry" {
                chrono::Duration::seconds(1)
            } else {
                chrono::Duration::hours(1)
            };
        sessions::create_session(&database.pool, change, admin.id, None, None, expires)
            .await
            .unwrap();
        let registration = transitions::register_password(
            &database.pool,
            "queued",
            None,
            "hash",
            Some((admin.id, change, false)),
        );
        tokio::pin!(registration);
        match &database.raw {
            Raw::Postgres(raw) => {
                let mut tx = raw.begin().await.unwrap();
                if change == "password" {
                    sqlx::query("UPDATE users SET password_hash='third-hash' WHERE id=$1")
                        .bind(admin.id)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                } else {
                    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
                        .bind(change)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                }
                let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                tokio::select! {
                    result = &mut registration => panic!("registration did not wait: {result:?}"),
                    _ = wait_for_pg_blocked(raw, holder) => {}
                }
                if change == "expiry" {
                    loop {
                        let expired: bool =
                            sqlx::query_scalar("SELECT $1::timestamptz<=clock_timestamp()")
                                .bind(expires)
                                .fetch_one(&mut *tx)
                                .await
                                .unwrap();
                        if expired {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } else {
                    sqlx::query("DELETE FROM sessions WHERE id=$1")
                        .bind(change)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                }
                tx.commit().await.unwrap();
            }
            Raw::Sqlite(raw) => {
                let mut tx = raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), &mut registration)
                        .await
                        .is_err(),
                    "registration must wait for the writer"
                );
                if change == "expiry" {
                    // Wait on the clock the registration check uses, not the Rust clock.
                    loop {
                        let expired: bool = sqlx::query_scalar(
                            "SELECT expires_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') FROM sessions WHERE id=?1",
                        )
                        .bind(change)
                        .fetch_one(&mut *tx)
                        .await
                        .unwrap();
                        if expired {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } else {
                    if change == "password" {
                        sqlx::query("UPDATE users SET password_hash='third-hash' WHERE id=?1")
                            .bind(admin.id)
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                    }
                    sqlx::query("DELETE FROM sessions WHERE id=?1")
                        .bind(change)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                }
                tx.commit().await.unwrap();
            }
        }
        assert!(matches!(
            registration.await.unwrap(),
            AccountAdmission::Denied
        ));
    }
    assert_eq!(accounts::count_users(&database.pool).await.unwrap(), 4);
    assert!(
        db::users::signup_requests::list_signup_requests(&database.pool, "pending", 100, 0, 0)
            .await
            .unwrap()
            .is_empty()
    );

    if let Raw::Postgres(raw) = &database.raw {
        // Pause insertion after authorization; logout must wait for its commit.
        sessions::create_session(
            &database.pool,
            "insertion",
            admin.id,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        sqlx::raw_sql("CREATE FUNCTION pause_managed_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(hashtext(current_schema()),37009); RETURN NEW; END $$; CREATE TRIGGER pause_managed_insert BEFORE INSERT ON users FOR EACH ROW EXECUTE FUNCTION pause_managed_insert();")
            .execute(raw).await.unwrap();
        let mut barrier = raw.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),37009)")
            .execute(&mut *barrier)
            .await
            .unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *barrier)
            .await
            .unwrap();
        let registration = transitions::register_password(
            &database.pool,
            "inserting",
            None,
            "hash",
            Some((admin.id, "insertion", false)),
        );
        tokio::pin!(registration);
        let creator = tokio::select! {
            result = &mut registration => panic!("insertion did not wait: {result:?}"),
            pid = wait_for_pg_blocked(raw, holder) => pid,
        };
        let logout = sessions::delete_session(&database.pool, "insertion");
        tokio::pin!(logout);
        tokio::select! {
            result = &mut logout => panic!("logout bypassed registration: {result:?}"),
            _ = wait_for_pg_blocked(raw, creator) => {}
        }
        barrier.commit().await.unwrap();
        let (created, logged_out) = tokio::join!(registration, logout);
        assert_eq!(admitted(created.unwrap()).username, "inserting");
        logged_out.unwrap();
        assert!(sessions::get_session(&database.pool, "insertion")
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_managed_account_requires_original_session() {
    let mut database = Database::postgres().await;
    let result = AssertUnwindSafe(managed_account_sessions(&database))
        .catch_unwind()
        .await;
    database.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_managed_account_requires_original_session() {
    let mut database = Database::sqlite().await;
    let result = AssertUnwindSafe(managed_account_sessions(&database))
        .catch_unwind()
        .await;
    database.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn suite(database: &Database) {
    let admin = setup_namespace_and_exact_recovery(database).await;
    primitive_paths_and_case_policy(database, admin.id).await;
    account_admission_and_existing_identity(database, admin.id).await;
    signup_review_atomicity(database, admin.id).await;
    concurrent_namespace_winners(database, admin.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_namespace_authority() {
    let mut database = Database::postgres().await;
    let result = AssertUnwindSafe(suite(&database)).catch_unwind().await;
    database.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_namespace_authority() {
    let mut database = Database::sqlite().await;
    let result = AssertUnwindSafe(suite(&database)).catch_unwind().await;
    database.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
