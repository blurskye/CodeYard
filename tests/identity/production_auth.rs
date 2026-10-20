//! Required PostgreSQL-only auth fixtures. No .env fallback and no silent skips.
use chrono::{Duration, Utc};
use codeyard::backend::{
    db::{
        self,
        users::{
            sessions as stored,
            transitions::{self, AccountAdmission, BanChange, SetupClaim},
        },
    },
    identity::{
        self,
        accounts::{ManagedAccountError, PasswordChange, SignupDecision, SignupReviewOutcome},
        admission,
        credentials::PasswordAccount,
        login::PasswordCredentials,
        passwords,
        registration::{RegistrationError, RegistrationOutcome},
        sessions::{AccountAuthorityError, InitiatingSession, SessionContext, SessionIssueError},
        setup::{SetupError, SetupInput},
        AuthenticatedAccount,
    },
};
use sha2::{Digest, Sha256};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Postgres, Transaction,
};
use std::{str::FromStr, time::Duration as StdDuration};
use uuid::Uuid;

/// The password step of a sign-in that has no second factor. Two-factor accounts are not used here.
fn signed_in(
    first: Result<identity::login::FirstFactor, identity::login::LoginError>,
) -> Result<AuthenticatedAccount, identity::login::LoginError> {
    match first? {
        identity::login::FirstFactor::Authenticated(account) => Ok(*account),
        identity::login::FirstFactor::SecondFactor { .. } => panic!("unexpected second factor"),
    }
}

#[test]
fn shared_identity_inputs_keep_key_profile_token_and_oauth_bounds() {
    use identity::{keys, oauth, profiles, tokens};
    assert_eq!(keys::validate_name(""), Err(keys::KeyError::InvalidName));
    assert!(keys::validate_name(&"x".repeat(100)).is_ok());
    assert_eq!(
        keys::validate_name(&"x".repeat(101)),
        Err(keys::KeyError::InvalidName)
    );

    let oversized = "x".repeat(4001);
    let profile = profiles::ProfileUpdate {
        display_name: None,
        bio: Some(&oversized),
        location: None,
        website: None,
    };
    assert_eq!(
        profiles::validate(&profile),
        Err(profiles::ProfileError::Invalid)
    );
    let profile = profiles::ProfileUpdate {
        bio: Some("public biography"),
        ..profile
    };
    assert!(profiles::validate(&profile).is_ok());
    assert!(!profiles::visible_on_profile(true, false));
    assert!(profiles::visible_on_profile(true, true));

    let now = Utc::now();
    let mut token = tokens::TokenInput {
        name: " Git client ",
        scope: "write",
        expires_at: Some(now + Duration::seconds(1)),
    };
    assert!(tokens::validate(&token, now).is_ok());
    token.expires_at = Some(now);
    assert_eq!(
        tokens::validate(&token, now),
        Err(tokens::TokenError::Invalid)
    );
    token.expires_at = None;
    token.scope = "admin";
    assert_eq!(
        tokens::validate(&token, now),
        Err(tokens::TokenError::Invalid)
    );
    assert_eq!(
        tokens::token_hash("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );

    assert!(oauth::validate_callback("provider-code", &"a".repeat(64)).is_ok());
    for (code, state) in [
        ("", "a".repeat(64)),
        ("code", "a".repeat(63)),
        ("code", "z".repeat(64)),
    ] {
        assert_eq!(
            oauth::validate_callback(code, &state),
            Err(oauth::OAuthError::InvalidCode)
        );
    }
}

const PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$cHJvZHVjdGlvbi1kdW1teQ$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
// Normal cryptographic fixtures must not consume each other's production KDF
// permits under an arbitrarily parallel test runner. Busy itself has an owner test.
use crate::support::PASSWORD_WORK as PASSWORD_FIXTURE_WORK;
fn random_id() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn binding(nonce: &str) -> String {
    hex::encode(Sha256::digest(nonce.as_bytes()))
}
fn session_context<'a>(fresh: &'a str, old: Option<&'a str>) -> SessionContext<'a> {
    SessionContext {
        initiating: old.map_or(InitiatingSession::Missing, InitiatingSession::Supplied),
        fresh_id: fresh,
        user_agent: Some("owned identity fixture"),
        expires_at: Utc::now() + Duration::days(1),
    }
}
fn test_database_url() -> String {
    let value = std::env::var("GITSERV_TEST_DATABASE_URL")
        .expect("auth tests require GITSERV_TEST_DATABASE_URL");
    let parsed = url::Url::parse(&value).expect("fixture URL");
    assert!(
        matches!(parsed.scheme(), "postgres" | "postgresql")
            && parsed.path() == "/gitserv_test"
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "fixture must use owned gitserv_test, no supplied options"
    );
    value
}

struct Database {
    pool: db::DbPool,
    raw: sqlx::PgPool,
    schema: String,
    url: String,
}
impl Drop for Database {
    fn drop(&mut self) {
        let fixture_pool = self.pool.clone();
        let schema = self.schema.clone();
        let url = self.url.clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    tokio::time::timeout(std::time::Duration::from_secs(10), fixture_pool.close())
                        .await
                        .map_err(|_| sqlx::Error::PoolTimedOut)?;
                    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
                    let pool = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(
                            options.options([("lock_timeout", "5s"), ("statement_timeout", "10s")]),
                        )
                        .await?;
                    let result = sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
                        .execute(&pool)
                        .await;
                    pool.close().await;
                    result.map(|_| ())
                })
        })
        .join();
        if !matches!(result, Ok(Ok(()))) {
            if std::thread::panicking() {
                eprintln!("owned auth schema cleanup failed: {}", self.schema);
            } else {
                panic!("owned auth schema cleanup failed: {}", self.schema);
            }
        }
    }
}
impl Database {
    async fn new() -> Self {
        let url = test_database_url();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("required PostgreSQL unavailable");
        let schema = format!("auth_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
            .execute(&admin)
            .await
            .expect("owned schema");
        let mut fixture = Self {
            pool: admin.clone().into(),
            raw: admin.clone(),
            schema,
            url,
        };
        admin.close().await;
        let options = PgConnectOptions::from_str(&fixture.url)
            .unwrap()
            .application_name(&fixture.schema)
            .options([("search_path", fixture.schema.as_str())]);
        fixture.raw = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .expect("owned schema pool");
        fixture.pool = fixture.raw.clone().into();
        db::run_migrations(&fixture.pool)
            .await
            .expect("required baseline migrations");
        fixture
    }
    async fn reconnect(&self) -> db::DbPool {
        let options = PgConnectOptions::from_str(&self.url)
            .unwrap()
            .application_name(&self.schema)
            .options([("search_path", self.schema.as_str())]);
        PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .expect("reconnect owned schema")
            .into()
    }
    async fn block_membership(&self) -> Transaction<'static, Postgres> {
        let mut tx = self.raw.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),37002)")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx
    }
    async fn wait_for_blocked(&self, count: i64) {
        tokio::time::timeout(StdDuration::from_secs(10), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock'")
                    .bind(&self.schema).fetch_one(&self.raw).await.unwrap();
                if waiting >= count { break; }
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        }).await.expect("requests must reach a real PostgreSQL lock barrier");
    }
    async fn setup(&self, mode: &str) -> db::models::User {
        match transitions::complete_setup(&self.pool, "admin", None, PHC, mode, None)
            .await
            .unwrap()
        {
            SetupClaim::Completed(user) => *user,
            other => panic!("unexpected setup: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_competitors_have_one_transactional_admin_and_mode() {
    let fixture = Database::new().await;
    let blocker = fixture.block_membership().await;
    let a_pool = fixture.pool.clone();
    let b_pool = fixture.pool.clone();
    let a = tokio::spawn(async move {
        transitions::complete_setup(&a_pool, "admin_a", None, PHC, "private", None)
            .await
            .unwrap()
    });
    let b = tokio::spawn(async move {
        transitions::complete_setup(&b_pool, "admin_b", None, PHC, "public", None)
            .await
            .unwrap()
    });
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    let outcomes = [a.await.unwrap(), b.await.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, SetupClaim::Completed(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, SetupClaim::AlreadyCompleted))
            .count(),
        1
    );
    let users = db::users::accounts::list_users(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(users.len(), 1);
    assert!(users[0].is_admin);
    let config = db::node_config::get_node_configuration(&fixture.pool)
        .await
        .unwrap()
        .unwrap();
    assert!(config.setup_completed);
    assert_eq!(
        config.node_mode,
        if users[0].username == "admin_a" {
            "private"
        } else {
            "public"
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_config_failure_rolls_back_and_original_admin_can_recover() {
    let fixture = Database::new().await;
    sqlx::raw_sql("CREATE FUNCTION reject_setup_config() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture config failure'; END $$; CREATE TRIGGER reject_setup_config BEFORE INSERT ON node_configuration FOR EACH ROW EXECUTE FUNCTION reject_setup_config();")
        .execute(&fixture.raw).await.unwrap();
    assert!(
        transitions::complete_setup(&fixture.pool, "admin", None, PHC, "private", None)
            .await
            .is_err()
    );
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        0
    );
    sqlx::raw_sql("DROP TRIGGER reject_setup_config ON node_configuration; DROP FUNCTION reject_setup_config();").execute(&fixture.raw).await.unwrap();
    // Provision the admin directly, leaving setup incomplete. Setup does not
    // auto-adopt arbitrary users.
    let orphan = db::users::accounts::create_user_with_password_and_flags(
        &fixture.pool,
        "admin",
        None,
        PHC,
        true,
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        transitions::complete_setup(&fixture.pool, "outsider", None, PHC, "public", None)
            .await
            .unwrap(),
        SetupClaim::ExistingAccounts
    ));
    assert!(matches!(
        transitions::complete_setup(
            &fixture.pool,
            "admin",
            None,
            PHC,
            "private",
            Some((orphan.id, "wrong"))
        )
        .await
        .unwrap(),
        SetupClaim::ExistingAccounts
    ));
    assert!(matches!(
        transitions::complete_setup(
            &fixture.pool,
            "admin",
            None,
            PHC,
            "private",
            Some((orphan.id, PHC))
        )
        .await
        .unwrap(),
        SetupClaim::Completed(_)
    ));
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        1
    );
    sqlx::raw_sql("CREATE FUNCTION reject_session() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture session failure'; END $$; CREATE TRIGGER reject_session BEFORE INSERT ON sessions FOR EACH ROW EXECUTE FUNCTION reject_session();")
        .execute(&fixture.raw).await.unwrap();
    assert!(stored::rotate_authenticated_session(
        &fixture.pool,
        None,
        &random_id(),
        orphan.id,
        Some(PHC),
        None,
        Utc::now() + Duration::days(1)
    )
    .await
    .is_err());
    assert!(
        db::node_config::get_node_configuration(&fixture.pool)
            .await
            .unwrap()
            .unwrap()
            .setup_completed
    );
    assert!(matches!(
        transitions::complete_setup(&fixture.pool, "new_admin", None, PHC, "public", None)
            .await
            .unwrap(),
        SetupClaim::AlreadyCompleted
    ));
    sqlx::raw_sql("DROP TRIGGER reject_session ON sessions; DROP FUNCTION reject_session();")
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(stored::rotate_authenticated_session(
        &fixture.pool,
        None,
        &random_id(),
        orphan.id,
        Some(PHC),
        None,
        Utc::now() + Duration::days(1)
    )
    .await
    .unwrap()
    .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reciprocal_admin_bans_revalidate_the_actor_and_preserve_repository_data() {
    let fixture = Database::new().await;
    let a = fixture.setup("private").await;
    let b = db::users::accounts::create_user_with_password_and_flags(
        &fixture.pool,
        "admin_b",
        None,
        PHC,
        true,
        false,
    )
    .await
    .unwrap();
    for id in [a.id, b.id] {
        sqlx::query("INSERT INTO repositories (owner_user_id,name,disk_path,is_private) VALUES ($1,'preserved','fixture-no-filesystem-effect',TRUE)")
            .bind(id).execute(&fixture.raw).await.unwrap();
        db::users::sessions::create_session(
            &fixture.pool,
            &random_id(),
            id,
            None,
            None,
            Utc::now() + Duration::days(1),
        )
        .await
        .unwrap();
    }
    let blocker = fixture.block_membership().await;
    let p = fixture.pool.clone();
    let q = fixture.pool.clone();
    let aid = a.id;
    let bid = b.id;
    let one =
        tokio::spawn(async move { transitions::change_ban(&p, aid, bid, true).await.unwrap() });
    let two =
        tokio::spawn(async move { transitions::change_ban(&q, bid, aid, true).await.unwrap() });
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    let outcomes = [one.await.unwrap(), two.await.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, BanChange::Updated(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, BanChange::Denied))
            .count(),
        1
    );
    assert_eq!(
        db::users::accounts::count_unbanned_admin_users(&fixture.pool)
            .await
            .unwrap(),
        1
    );
    let repos: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repositories")
        .fetch_one(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(repos, 2);
    let banned_sessions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions s JOIN users u ON u.id=s.user_id WHERE u.is_banned",
    )
    .fetch_one(&fixture.raw)
    .await
    .unwrap();
    assert_eq!(banned_sessions, 0);
    assert!(matches!(
        transitions::change_ban(&fixture.pool, aid, aid, true)
            .await
            .unwrap(),
        BanChange::SelfBan | BanChange::Denied
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_rejects_collisions_and_rebind_persists_across_pools_and_revokes_old_csrf() {
    let fixture = Database::new().await;
    let user = fixture.setup("public").await;
    let old = random_id();
    let fresh = random_id();
    let expiry = Utc::now() + Duration::days(1);
    stored::create_browser_session(&fixture.pool, &old, None, None, expiry)
        .await
        .unwrap();
    assert!(
        stored::create_browser_session(&fixture.pool, &old, None, None, expiry)
            .await
            .is_err(),
        "collision cannot silently succeed"
    );
    assert!(
        stored::create_browser_session(&fixture.pool, "caller-picked", None, None, expiry)
            .await
            .is_err()
    );
    let old_token = random_id();
    assert_eq!(
        stored::get_or_create_csrf_token(&fixture.pool, &old, None, &old_token, 7200)
            .await
            .unwrap(),
        Some(old_token.clone())
    );
    let issued = stored::rotate_authenticated_session(
        &fixture.pool,
        Some(&old),
        &fresh,
        user.id,
        Some(PHC),
        None,
        expiry,
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(issued.id, old);
    assert_eq!(issued.user_id, Some(user.id));
    assert!(stored::get_browser_session(&fixture.pool, &old)
        .await
        .unwrap()
        .is_none());
    assert!(
        stored::get_or_create_csrf_token(&fixture.pool, &old, None, &random_id(), 7200)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        stored::create_browser_session(&fixture.pool, &fresh, None, None, expiry)
            .await
            .is_err()
    );
    let token = random_id();
    stored::get_or_create_csrf_token(&fixture.pool, &fresh, Some(user.id), &token, 7200)
        .await
        .unwrap();
    let restarted = fixture.reconnect().await;
    assert_eq!(
        stored::get_browser_session(&restarted, &fresh)
            .await
            .unwrap()
            .unwrap()
            .user_id,
        Some(user.id)
    );
    assert_eq!(
        stored::get_or_create_csrf_token(&restarted, &fresh, Some(user.id), &random_id(), 7200)
            .await
            .unwrap(),
        Some(token)
    );
    restarted.close().await;
    let unknown = random_id();
    let other = random_id();
    assert_ne!(
        stored::rotate_authenticated_session(
            &fixture.pool,
            Some(&unknown),
            &other,
            user.id,
            Some(PHC),
            None,
            expiry
        )
        .await
        .unwrap()
        .unwrap()
        .id,
        unknown
    );
    assert!(stored::get_browser_session(&fixture.pool, &unknown)
        .await
        .unwrap()
        .is_none());
    assert!(stored::rotate_authenticated_session(
        &fixture.pool,
        Some(&fresh),
        &random_id(),
        user.id,
        Some(PHC),
        None,
        expiry
    )
    .await
    .unwrap()
    .is_none());
    assert!(stored::rotate_authenticated_session(
        &fixture.pool,
        None,
        &random_id(),
        user.id,
        Some("stale-hash"),
        None,
        expiry
    )
    .await
    .unwrap()
    .is_none());
    let expired = random_id();
    stored::create_browser_session(&fixture.pool, &expired, None, None, expiry)
        .await
        .unwrap();
    sqlx::query("UPDATE anonymous_sessions SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1")
        .bind(&expired)
        .execute(&fixture.raw)
        .await
        .unwrap();
    let renewed = random_id();
    stored::rotate_authenticated_session(
        &fixture.pool,
        Some(&expired),
        &renewed,
        user.id,
        Some(PHC),
        None,
        expiry,
    )
    .await
    .unwrap()
    .unwrap();
    stored::delete_session(&fixture.pool, &fresh).await.unwrap();
    assert!(stored::get_browser_session(&fixture.pool, &fresh)
        .await
        .unwrap()
        .is_none());
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &fresh,
        Some(user.id),
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_none());
    for id in [old, fresh, other, renewed] {
        stored::delete_session(&fixture.pool, &id).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_state_persists_in_fresh_processes() {
    // The same compiled test is restarted twice in separate OS processes. This
    // proves auth-store restart persistence, not full HTTP/CI service lifecycle.
    if let Ok(schema) = std::env::var("GITSERV_AUTH_CHILD_SCHEMA") {
        assert!(
            schema.starts_with("auth_")
                && schema.len() == 37
                && schema[5..].bytes().all(|byte| byte.is_ascii_hexdigit())
        );
        let url = test_database_url();
        let options = PgConnectOptions::from_str(&url)
            .unwrap()
            .options([("search_path", schema.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let pool: db::DbPool = pool.into();
        let id = std::env::var("GITSERV_AUTH_CHILD_ID").expect("parent fixture session");
        let token = std::env::var("GITSERV_AUTH_CHILD_CSRF").expect("parent fixture CSRF");
        let user: i64 = std::env::var("GITSERV_AUTH_CHILD_USER")
            .expect("parent fixture identity")
            .parse()
            .unwrap();
        assert_eq!(
            stored::get_browser_session(&pool, &id)
                .await
                .unwrap()
                .unwrap()
                .user_id,
            Some(user)
        );
        assert_eq!(
            stored::get_or_create_csrf_token(&pool, &id, Some(user), &random_id(), 7200)
                .await
                .unwrap(),
            Some(token)
        );
        assert_eq!(
            admission::admit_login(&pool, Some(user), "alias-does-not-reset-admission").await,
            Err(admission::AdmissionError::Limited)
        );
        pool.close().await;
    } else {
        let fixture = Database::new().await;
        let user = fixture.setup("public").await;
        let id = random_id();
        let token = random_id();
        stored::rotate_authenticated_session(
            &fixture.pool,
            None,
            &id,
            user.id,
            Some(PHC),
            None,
            Utc::now() + Duration::days(1),
        )
        .await
        .unwrap()
        .unwrap();
        stored::get_or_create_csrf_token(&fixture.pool, &id, Some(user.id), &token, 7200)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..10 {
            admission::admit_login(&fixture.pool, Some(user.id), "username")
                .await
                .unwrap();
        }
        for _ in 0..2 {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "production_auth::authentication_state_persists_in_fresh_processes",
                    "--test-threads=1",
                ])
                .env("GITSERV_AUTH_CHILD_SCHEMA", &fixture.schema)
                .env("GITSERV_AUTH_CHILD_ID", &id)
                .env("GITSERV_AUTH_CHILD_CSRF", &token)
                .env("GITSERV_AUTH_CHILD_USER", user.id.to_string())
                .output()
                .expect("restart owned auth fixture process");
            assert!(
                output.status.success(),
                "auth child fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "child must actually execute its auth assertions"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_csrf_creation_and_expiry_share_one_token_bound_to_live_identity() {
    let fixture = Database::new().await;
    let user = fixture.setup("public").await;
    let id = random_id();
    stored::create_browser_session(
        &fixture.pool,
        &id,
        None,
        None,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    let mut blocker = fixture.raw.begin().await.unwrap();
    sqlx::query("SELECT id FROM anonymous_sessions WHERE id=$1 FOR UPDATE")
        .bind(&id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let p = fixture.pool.clone();
    let q = fixture.pool.clone();
    let x = id.clone();
    let y = id.clone();
    let one = tokio::spawn(async move {
        stored::get_or_create_csrf_token(&p, &x, None, &random_id(), 7200)
            .await
            .unwrap()
            .unwrap()
    });
    let two = tokio::spawn(async move {
        stored::get_or_create_csrf_token(&q, &y, None, &random_id(), 7200)
            .await
            .unwrap()
            .unwrap()
    });
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    let token = one.await.unwrap();
    assert_eq!(two.await.unwrap(), token);
    assert_eq!(
        stored::get_or_create_csrf_token(&fixture.pool, &id, None, &random_id(), 7200)
            .await
            .unwrap(),
        Some(token.clone())
    );
    let clipped: bool = sqlx::query_scalar(
        "SELECT csrf_expires_at<=expires_at FROM anonymous_sessions WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&fixture.raw)
    .await
    .unwrap();
    assert!(clipped);
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &id,
        Some(user.id),
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_none());
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &random_id(),
        None,
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_none());
    assert!(
        stored::get_or_create_csrf_token(&fixture.pool, &id, None, "invalid", 7200)
            .await
            .is_err()
    );
    assert!(
        stored::get_or_create_csrf_token(&fixture.pool, &id, None, &random_id(), 7201)
            .await
            .is_err()
    );
    // Commit the expiry first: until then the committed token is still valid and a
    // request answers it without waiting. The replacement waits on the row lock below.
    sqlx::query(
        "UPDATE anonymous_sessions SET csrf_expires_at=NOW()-INTERVAL '1 second' WHERE id=$1",
    )
    .bind(&id)
    .execute(&fixture.raw)
    .await
    .unwrap();
    let mut blocker = fixture.raw.begin().await.unwrap();
    sqlx::query("SELECT id FROM anonymous_sessions WHERE id=$1 FOR UPDATE")
        .bind(&id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let p = fixture.pool.clone();
    let q = fixture.pool.clone();
    let x = id.clone();
    let y = id.clone();
    let one = tokio::spawn(async move {
        stored::get_or_create_csrf_token(&p, &x, None, &random_id(), 7200)
            .await
            .unwrap()
            .unwrap()
    });
    let two = tokio::spawn(async move {
        stored::get_or_create_csrf_token(&q, &y, None, &random_id(), 7200)
            .await
            .unwrap()
            .unwrap()
    });
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    let replacement = one.await.unwrap();
    assert_ne!(replacement, token);
    assert_eq!(two.await.unwrap(), replacement);
    let auth = random_id();
    stored::rotate_authenticated_session(
        &fixture.pool,
        Some(&id),
        &auth,
        user.id,
        Some(PHC),
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        stored::get_or_create_csrf_token(&fixture.pool, &auth, None, &random_id(), 7200)
            .await
            .unwrap()
            .is_none()
    );
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &auth,
        Some(user.id + 1),
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_none());
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &auth,
        Some(user.id),
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_some());
    db::users::accounts::update_user_password(&fixture.pool, user.id, PHC, false)
        .await
        .unwrap();
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &auth,
        Some(user.id),
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_none());
    let expired = random_id();
    stored::create_browser_session(
        &fixture.pool,
        &expired,
        None,
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap();
    stored::get_or_create_csrf_token(&fixture.pool, &expired, None, &random_id(), 7200)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE anonymous_sessions SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1")
        .bind(&expired)
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(stored::get_browser_session(&fixture.pool, &expired)
        .await
        .unwrap()
        .is_none());
    assert!(
        stored::get_or_create_csrf_token(&fixture.pool, &expired, None, &random_id(), 7200)
            .await
            .unwrap()
            .is_none()
    );
    let failed = fixture.reconnect().await;
    failed.close().await;
    assert!(stored::get_browser_session(&failed, &expired)
        .await
        .is_err());
    assert!(
        stored::get_or_create_csrf_token(&failed, &expired, None, &random_id(), 7200)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_rotation_never_promotes_the_initiating_id_or_rebinds_an_active_id() {
    let fixture = Database::new().await;
    let user = fixture.setup("public").await;
    let old = random_id();
    let expiry = Utc::now() + Duration::days(1);
    stored::create_browser_session(&fixture.pool, &old, None, None, expiry)
        .await
        .unwrap();
    let mut blocker = fixture.raw.begin().await.unwrap();
    // Use the same shared lock as issuance; both requests must wait in PostgreSQL.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),hashtext('browser-session:' || $1))").bind(&old).execute(&mut *blocker).await.unwrap();
    let p = fixture.pool.clone();
    let q = fixture.pool.clone();
    let x = old.clone();
    let y = old.clone();
    let uid = user.id;
    let one = tokio::spawn(async move {
        stored::rotate_authenticated_session(
            &p,
            Some(&x),
            &random_id(),
            uid,
            Some(PHC),
            None,
            expiry,
        )
        .await
        .unwrap()
        .unwrap()
    });
    let two = tokio::spawn(async move {
        stored::rotate_authenticated_session(
            &q,
            Some(&y),
            &random_id(),
            uid,
            Some(PHC),
            None,
            expiry,
        )
        .await
        .unwrap()
        .unwrap()
    });
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    let a = one.await.unwrap();
    let b = two.await.unwrap();
    assert_ne!(a.id, b.id);
    assert_ne!(a.id, old);
    assert_ne!(b.id, old);
    assert!(stored::get_browser_session(&fixture.pool, &old)
        .await
        .unwrap()
        .is_none());
    assert!(stored::rotate_authenticated_session(
        &fixture.pool,
        Some(&a.id),
        &random_id(),
        uid,
        Some(PHC),
        None,
        expiry
    )
    .await
    .unwrap()
    .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_state_requires_browser_provider_origin_and_one_atomic_consumer() {
    let fixture = Database::new().await;
    let state = random_id();
    let browser = binding(&random_id());
    let redirect = "https://git.example/api/auth/oauth/google/callback";
    stored::save_bound_oauth_state(
        &fixture.pool,
        &state,
        &random_id(),
        "google",
        redirect,
        &browser,
        "/alice/project/issues/7?tab=comments",
    )
    .await
    .unwrap();
    assert!(stored::consume_bound_oauth_state(
        &fixture.pool,
        &state,
        "google",
        &binding(&random_id()),
        redirect
    )
    .await
    .unwrap()
    .is_none());
    assert!(
        stored::consume_bound_oauth_state(&fixture.pool, &state, "github", &browser, redirect)
            .await
            .unwrap()
            .is_none()
    );
    assert!(stored::consume_bound_oauth_state(
        &fixture.pool,
        &state,
        "google",
        &browser,
        "https://evil.example/callback"
    )
    .await
    .unwrap()
    .is_none());
    let mut blocker = fixture.raw.begin().await.unwrap();
    sqlx::query("SELECT state FROM oauth_states WHERE state=$1 FOR UPDATE")
        .bind(&state)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let p = fixture.pool.clone();
    let q = fixture.pool.clone();
    let x = state.clone();
    let y = state.clone();
    let a = browser.clone();
    let b = browser.clone();
    let one = tokio::spawn(async move {
        stored::consume_bound_oauth_state(&p, &x, "google", &a, redirect)
            .await
            .unwrap()
    });
    let two = tokio::spawn(async move {
        stored::consume_bound_oauth_state(&q, &y, "google", &b, redirect)
            .await
            .unwrap()
    });
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    let one = one.await.unwrap();
    let two = two.await.unwrap();
    assert_ne!(one.is_some(), two.is_some());
    assert_eq!(
        one.or(two).unwrap().return_to,
        "/alice/project/issues/7?tab=comments"
    );
    assert!(
        stored::consume_bound_oauth_state(&fixture.pool, &state, "google", &browser, redirect)
            .await
            .unwrap()
            .is_none()
    );
    let expired = random_id();
    stored::save_bound_oauth_state(
        &fixture.pool,
        &expired,
        "",
        "google",
        redirect,
        &browser,
        "/",
    )
    .await
    .unwrap();
    sqlx::query("UPDATE oauth_states SET expires_at=NOW()-INTERVAL '1 second' WHERE state=$1")
        .bind(&expired)
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(stored::consume_bound_oauth_state(
        &fixture.pool,
        &expired,
        "google",
        &browser,
        redirect
    )
    .await
    .unwrap()
    .is_none());
    let unbound = random_id();
    stored::save_oauth_state(&fixture.pool, &unbound, "", "google", Some(redirect))
        .await
        .unwrap();
    assert!(stored::consume_bound_oauth_state(
        &fixture.pool,
        &unbound,
        "google",
        &browser,
        redirect
    )
    .await
    .unwrap()
    .is_none());
    // Running the migrations again changes nothing and keeps existing state.
    db::run_migrations(&fixture.pool).await.unwrap();
    assert!(
        stored::get_oauth_state(&fixture.pool, &unbound)
            .await
            .unwrap()
            .is_some(),
        "repeated migration preserves existing state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_admission_never_bootstraps_or_bypasses_private_approval_or_password_policy() {
    let fixture = Database::new().await;
    assert!(matches!(
        transitions::admit_oauth_account(&fixture.pool, "google", "subject", "outsider", None)
            .await
            .unwrap(),
        AccountAdmission::SetupRequired
    ));
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        0
    );
    let admin = fixture.setup("private").await;
    assert!(matches!(
        transitions::admit_oauth_account(&fixture.pool, "google", "subject", "outsider", None)
            .await
            .unwrap(),
        AccountAdmission::ApprovalRequired
    ));
    let approved = db::users::accounts::create_user_with_password_and_flags(
        &fixture.pool,
        "approved",
        None,
        PHC,
        false,
        true,
    )
    .await
    .unwrap();
    assert!(!transitions::enroll_oauth_identity(
        &fixture.pool,
        approved.id,
        approved.id,
        "google",
        "subject"
    )
    .await
    .unwrap());
    assert!(transitions::enroll_oauth_identity(
        &fixture.pool,
        admin.id,
        approved.id,
        "google",
        "subject"
    )
    .await
    .unwrap());
    assert!(matches!(
        transitions::admit_oauth_account(&fixture.pool, "google", "subject", "ignored", None)
            .await
            .unwrap(),
        AccountAdmission::Denied
    ));
    assert!(stored::rotate_authenticated_session(
        &fixture.pool,
        None,
        &random_id(),
        approved.id,
        None,
        None,
        Utc::now() + Duration::days(1)
    )
    .await
    .unwrap()
    .is_none());
    db::users::accounts::update_user_password(&fixture.pool, approved.id, PHC, false)
        .await
        .unwrap();
    assert!(
        matches!(transitions::admit_oauth_account(&fixture.pool,"google","subject","ignored",None).await.unwrap(),AccountAdmission::User(user) if user.id==approved.id)
    );
    assert!(matches!(
        transitions::change_ban(&fixture.pool, admin.id, approved.id, true)
            .await
            .unwrap(),
        BanChange::Updated(_)
    ));
    assert!(matches!(
        transitions::admit_oauth_account(&fixture.pool, "google", "subject", "ignored", None)
            .await
            .unwrap(),
        AccountAdmission::Denied
    ));
    assert!(stored::rotate_authenticated_session(
        &fixture.pool,
        None,
        &random_id(),
        approved.id,
        Some(PHC),
        None,
        Utc::now() + Duration::days(1)
    )
    .await
    .unwrap()
    .is_none());
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_session_revocation_failure_rolls_back_both_hash_and_required_change_flag() {
    let _password_work = PASSWORD_FIXTURE_WORK.lock().await;
    let fixture = Database::new().await;
    fixture.setup("private").await;
    let user = db::users::accounts::create_user_with_password_and_flags(
        &fixture.pool,
        "temporary",
        None,
        PHC,
        false,
        true,
    )
    .await
    .unwrap();
    let id = random_id();
    stored::rotate_authenticated_session(
        &fixture.pool,
        None,
        &id,
        user.id,
        Some(PHC),
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap()
    .unwrap();
    stored::get_or_create_csrf_token(&fixture.pool, &id, Some(user.id), &random_id(), 7200)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored::get_session_user(&fixture.pool, &id)
            .await
            .unwrap()
            .is_none(),
        "temporary credential grants no ordinary resource authority"
    );
    assert!(
        stored::get_session_account(&fixture.pool, &id)
            .await
            .unwrap()
            .is_some(),
        "password-change exception retains current identity"
    );
    sqlx::raw_sql("CREATE FUNCTION reject_revocation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture revocation failure'; END $$; CREATE TRIGGER reject_revocation BEFORE DELETE ON sessions FOR EACH ROW EXECUTE FUNCTION reject_revocation();")
        .execute(&fixture.raw).await.unwrap();
    let replacement = passwords::hash_password("replacement-password")
        .await
        .unwrap();
    assert!(transitions::compare_exchange_password(
        &fixture.pool,
        user.id,
        PHC,
        &replacement,
        true,
        true,
        None,
    )
    .await
    .is_err());
    let unchanged = db::users::accounts::get_user_by_id(&fixture.pool, user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.password_hash.as_deref(), Some(PHC));
    assert!(unchanged.must_change_password);
    assert!(stored::get_browser_session(&fixture.pool, &id)
        .await
        .unwrap()
        .is_some());
    sqlx::raw_sql("DROP TRIGGER reject_revocation ON sessions; DROP FUNCTION reject_revocation();")
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(transitions::compare_exchange_password(
        &fixture.pool,
        user.id,
        PHC,
        &replacement,
        true,
        true,
        None,
    )
    .await
    .unwrap()
    .is_some());
    assert!(stored::get_browser_session(&fixture.pool, &id)
        .await
        .unwrap()
        .is_none());
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &id,
        Some(user.id),
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_none());
    assert!(
        !db::users::accounts::get_user_by_id(&fixture.pool, user.id)
            .await
            .unwrap()
            .unwrap()
            .must_change_password
    );
}

/// The token insert waits behind a password change's account lock. Once the lock is released
/// the revocation has committed, so the insert is refused and leaves no token behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_insert_waiting_behind_password_revocation_is_refused() {
    let fixture = Database::new().await;
    let user = db::users::accounts::create_user_with_password_and_flags(
        &fixture.pool,
        "tokenrace",
        None,
        PHC,
        false,
        false,
    )
    .await
    .unwrap();
    let session = random_id();
    stored::create_session(
        &fixture.pool,
        &session,
        user.id,
        None,
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap();
    // The password change holds the account row and has not revoked anything yet.
    let mut change = fixture.raw.begin().await.unwrap();
    sqlx::query(
        "UPDATE users SET password_hash='replacement', must_change_password=FALSE WHERE id=$1",
    )
    .bind(user.id)
    .execute(&mut *change)
    .await
    .unwrap();
    let (pool, user_id) = (fixture.pool.clone(), user.id);
    let racing = tokio::spawn(async move {
        identity::tokens::create(
            &pool,
            user_id,
            Some(session.as_str()),
            identity::tokens::TokenInput {
                name: "racing",
                scope: "write",
                expires_at: None,
            },
        )
        .await
    });
    fixture.wait_for_blocked(1).await;
    sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=$1")
        .bind(user.id)
        .execute(&mut *change)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sessions WHERE user_id=$1")
        .bind(user.id)
        .execute(&mut *change)
        .await
        .unwrap();
    change.commit().await.unwrap();
    assert!(matches!(
        racing.await.unwrap(),
        Err(identity::tokens::TokenError::Denied)
    ));
    assert!(db::users::tokens::list_tokens(&fixture.pool, user.id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_oauth_parallel_enrollment_has_no_orphan_user_or_second_link() {
    let fixture = Database::new().await;
    fixture.setup("public").await;
    let blocker = fixture.block_membership().await;
    let p = fixture.pool.clone();
    let q = fixture.pool.clone();
    let one = tokio::spawn(async move {
        transitions::admit_oauth_account(&p, "github", "42", "same", None)
            .await
            .unwrap()
    });
    let two = tokio::spawn(async move {
        transitions::admit_oauth_account(&q, "github", "42", "same", None)
            .await
            .unwrap()
    });
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    let AccountAdmission::User(a) = one.await.unwrap() else {
        panic!("public enrollment")
    };
    let AccountAdmission::User(b) = two.await.unwrap() else {
        panic!("linked enrollment")
    };
    assert_eq!(a.id, b.id);
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        2
    );
    let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_accounts")
        .fetch_one(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(links, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_admission_is_shared_atomic_alias_safe_bounded_and_fail_closed() {
    let fixture = Database::new().await;
    let second = fixture.reconnect().await;
    let mut blocker = fixture.raw.begin().await.unwrap();
    sqlx::query("INSERT INTO auth_login_limits (account_key,attempts,window_ends_at) VALUES ('global',0,NOW())").execute(&mut *blocker).await.unwrap();
    let p = fixture.pool.clone();
    let q = second.clone();
    let a = tokio::spawn(async move { admission::admit_login(&p, Some(42), "username").await });
    let b = tokio::spawn(
        async move { admission::admit_login(&q, Some(42), "alias@example.com").await },
    );
    fixture.wait_for_blocked(2).await;
    blocker.commit().await.unwrap();
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    let mut tasks = Vec::new();
    for attempt in 0..18 {
        let pool = if attempt % 2 == 0 {
            fixture.pool.clone()
        } else {
            second.clone()
        };
        tasks.push(tokio::spawn(async move {
            admission::admit_login(
                &pool,
                Some(42),
                if attempt % 2 == 0 {
                    "username"
                } else {
                    "alias@example.com"
                },
            )
            .await
        }));
    }
    let mut admitted = 2;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => admitted += 1,
            Err(admission::AdmissionError::Limited) => {}
            other => panic!("unexpected fixture limiter: {other:?}"),
        }
    }
    assert_eq!(admitted, 10);
    for account in 0..100 {
        admission::admit_login(&fixture.pool, None, &format!("unknown-{account}"))
            .await
            .unwrap();
    }
    let identity = "never-allocate-this-key";
    assert_eq!(
        admission::admit_login(&second, None, identity).await,
        Err(admission::AdmissionError::Limited)
    );
    let nonexistent: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM auth_login_limits WHERE account_key=$1)")
            .bind(format!("unknown:{}", binding(identity)))
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert!(
        !nonexistent,
        "global denial must not allocate identity state"
    );
    let bounded: bool = sqlx::query_scalar("SELECT attempts=10 AND window_ends_at<=clock_timestamp()+INTERVAL '300 seconds' FROM auth_login_limits WHERE account_key='user:42'").fetch_one(&fixture.raw).await.unwrap();
    assert!(bounded);
    // Prove bounded cleanup touches only expired new limiter rows, never current state.
    sqlx::query("INSERT INTO auth_login_limits (account_key,attempts,window_ends_at) SELECT 'expired:' || n,1,NOW()-INTERVAL '1 second' FROM generate_series(1,40) n").execute(&fixture.raw).await.unwrap();
    sqlx::query("UPDATE auth_login_limits SET window_ends_at=NOW()-INTERVAL '1 second' WHERE account_key='global'").execute(&fixture.raw).await.unwrap();
    admission::admit_login(&fixture.pool, None, "after-window")
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_login_limits WHERE account_key LIKE 'expired:%'",
    )
    .fetch_one(&fixture.raw)
    .await
    .unwrap();
    assert_eq!(remaining, 8, "one cleanup batch is limited to 32 rows");
    let current: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_login_limits WHERE account_key='user:42'")
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(current, 1);
    second.close().await;
    assert_eq!(
        admission::admit_login(&second, Some(42), "ignored").await,
        Err(admission::AdmissionError::Unavailable)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_login_owns_alias_admission_and_anonymous_csrf_rotation() {
    let _password_work = PASSWORD_FIXTURE_WORK.lock().await;
    let fixture = Database::new().await;
    fixture.setup("public").await;
    let hash = passwords::hash_password("alias-password").await.unwrap();
    let user = db::users::accounts::create_user_with_password_and_flags(
        &fixture.pool,
        "alias",
        Some("alias@example.test"),
        &hash,
        false,
        false,
    )
    .await
    .unwrap();
    let old = random_id();
    let fresh = random_id();
    let old_csrf = random_id();
    stored::create_browser_session(
        &fixture.pool,
        &old,
        None,
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap();
    stored::get_or_create_csrf_token(&fixture.pool, &old, None, &old_csrf, 7200)
        .await
        .unwrap()
        .unwrap();
    let authenticated = signed_in(
        identity::login::password_login(
            &fixture.pool,
            None,
            PasswordCredentials {
                identity: "alias@example.test",
                password: "alias-password",
            },
            session_context(&fresh, Some(&old)),
        )
        .await,
    )
    .unwrap();
    assert_eq!(authenticated.user.id, user.id);
    assert_eq!(authenticated.session_id, fresh);
    assert!(stored::get_browser_session(&fixture.pool, &old)
        .await
        .unwrap()
        .is_none());
    assert!(
        stored::get_or_create_csrf_token(&fixture.pool, &old, None, &random_id(), 7200)
            .await
            .unwrap()
            .is_none()
    );
    // The operation charged the durable account rather than the email spelling.
    let attempts: i32 =
        sqlx::query_scalar("SELECT attempts FROM auth_login_limits WHERE account_key=$1")
            .bind(format!("user:{}", user.id))
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    let other = random_id();
    assert!(matches!(
        signed_in(
            identity::login::password_login(
                &fixture.pool,
                None,
                PasswordCredentials {
                    identity: "alias",
                    password: "alias-password"
                },
                session_context(&other, Some(&fresh)),
            )
            .await
        ),
        Err(identity::login::LoginError::Session(
            SessionIssueError::Denied
        ))
    ));
    assert!(stored::get_browser_session(&fixture.pool, &fresh)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_setup_and_registration_session_outages_leave_committed_accounts_retryable() {
    let _password_work = PASSWORD_FIXTURE_WORK.lock().await;
    let fixture = Database::new().await;
    let fresh = random_id();
    assert!(matches!(
        identity::setup::complete_setup(
            &fixture.pool,
            SetupInput {
                account: PasswordAccount {
                    username: "root_admin",
                    email: None,
                    password: "setup-password"
                },
                node_mode: "public",
                admin_secret: Some("wrong"),
            },
            Some("bootstrap-secret"),
            session_context(&fresh, None),
        )
        .await,
        Err(SetupError::InvalidAdminSecret)
    ));
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        0
    );
    sqlx::raw_sql("CREATE FUNCTION reject_identity_session() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture session failure'; END $$; CREATE TRIGGER reject_identity_session BEFORE INSERT ON sessions FOR EACH ROW EXECUTE FUNCTION reject_identity_session();")
        .execute(&fixture.raw).await.unwrap();
    assert!(matches!(
        identity::setup::complete_setup(
            &fixture.pool,
            SetupInput {
                account: PasswordAccount {
                    username: "root_admin",
                    email: None,
                    password: "setup-password"
                },
                node_mode: " PUBLIC ",
                admin_secret: Some(" bootstrap-secret "),
            },
            Some("bootstrap-secret"),
            session_context(&fresh, None),
        )
        .await,
        Err(SetupError::Session(SessionIssueError::Unavailable))
    ));
    assert!(
        identity::setup::status(&fixture.pool)
            .await
            .unwrap()
            .setup_completed
    );
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        1
    );
    let registered_session = random_id();
    assert!(matches!(
        identity::registration::register_password(
            &fixture.pool,
            PasswordAccount {
                username: "registered",
                email: None,
                password: "registered-password"
            },
            session_context(&registered_session, None),
        )
        .await,
        Err(RegistrationError::Session(SessionIssueError::Unavailable))
    ));
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        2
    );
    sqlx::raw_sql("DROP TRIGGER reject_identity_session ON sessions; DROP FUNCTION reject_identity_session();")
        .execute(&fixture.raw).await.unwrap();
    for (name, password) in [
        ("root_admin", "setup-password"),
        ("registered", "registered-password"),
    ] {
        let fresh = random_id();
        assert_eq!(
            signed_in(
                identity::login::password_login(
                    &fixture.pool,
                    None,
                    PasswordCredentials {
                        identity: name,
                        password
                    },
                    session_context(&fresh, None),
                )
                .await
            )
            .unwrap()
            .user
            .username,
            name
        );
    }
    // Invalid decoded context remains an issuance error *after* the account commit.
    let invalid_session = random_id();
    let mut context = session_context(&invalid_session, None);
    context.initiating = InitiatingSession::Invalid;
    assert!(matches!(
        identity::registration::register_password(
            &fixture.pool,
            PasswordAccount {
                username: "invalid_context",
                email: None,
                password: "registered-password"
            },
            context,
        )
        .await,
        Err(RegistrationError::Session(
            SessionIssueError::InvalidInitiator
        ))
    ));
    assert!(
        db::users::accounts::get_user_by_username(&fixture.pool, "invalid_context")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_private_registration_managed_password_change_and_review_share_authority() {
    let _password_work = PASSWORD_FIXTURE_WORK.lock().await;
    let fixture = Database::new().await;
    let admin_session = random_id();
    let admin = identity::setup::complete_setup(
        &fixture.pool,
        SetupInput {
            account: PasswordAccount {
                username: "root_admin",
                email: None,
                password: "admin-password",
            },
            node_mode: "private",
            admin_secret: None,
        },
        None,
        session_context(&admin_session, None),
    )
    .await
    .unwrap();
    let unused = random_id();
    assert!(matches!(
        identity::registration::register_password(
            &fixture.pool,
            PasswordAccount {
                username: "requested",
                email: None,
                password: "requested-password"
            },
            session_context(&unused, None),
        )
        .await
        .unwrap(),
        RegistrationOutcome::Requested
    ));
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        1
    );
    assert!(stored::get_browser_session(&fixture.pool, &unused)
        .await
        .unwrap()
        .is_none());
    let temporary = identity::accounts::create_managed_account(
        &fixture.pool,
        Some(&admin_session),
        PasswordAccount {
            username: "temporary",
            email: None,
            password: "temporary-password",
        },
        false,
    )
    .await
    .unwrap();
    assert!(temporary.must_change_password && !temporary.is_admin);
    let temporary_session = random_id();
    signed_in(
        identity::login::password_login(
            &fixture.pool,
            None,
            PasswordCredentials {
                identity: "temporary",
                password: "temporary-password",
            },
            session_context(&temporary_session, None),
        )
        .await,
    )
    .unwrap();
    assert!(stored::get_session_user(&fixture.pool, &temporary_session)
        .await
        .unwrap()
        .is_none());
    stored::get_or_create_csrf_token(
        &fixture.pool,
        &temporary_session,
        Some(temporary.id),
        &random_id(),
        7200,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        identity::accounts::create_managed_account(
            &fixture.pool,
            Some(&temporary_session),
            PasswordAccount {
                username: "denied",
                email: None,
                password: "denied-password"
            },
            false,
        )
        .await,
        Err(ManagedAccountError::Authority(
            AccountAuthorityError::PasswordChangeRequired
        ))
    ));
    let changed = identity::accounts::change_password(
        &fixture.pool,
        Some(&temporary_session),
        PasswordChange {
            current_password: "temporary-password",
            new_password: "replacement-password",
        },
    )
    .await
    .unwrap();
    assert!(!changed.must_change_password);
    assert!(
        stored::get_browser_session(&fixture.pool, &temporary_session)
            .await
            .unwrap()
            .is_none()
    );
    assert!(stored::get_or_create_csrf_token(
        &fixture.pool,
        &temporary_session,
        Some(temporary.id),
        &random_id(),
        7200
    )
    .await
    .unwrap()
    .is_none());
    let request =
        db::users::signup_requests::list_signup_requests(&fixture.pool, "pending", 100, 0, 0)
            .await
            .unwrap()
            .remove(0);
    assert!(matches!(identity::accounts::review_signup(
        &fixture.pool, Some(&admin_session), request.id, Some(SignupDecision::Approve),
    ).await.unwrap(), SignupReviewOutcome::Approved(user) if user.username == "requested"));
    assert!(matches!(
        identity::accounts::review_signup(
            &fixture.pool,
            Some(&admin_session),
            request.id,
            Some(SignupDecision::Approve),
        )
        .await,
        Err(identity::accounts::SignupReviewError::Processed)
    ));
    assert!(matches!(
        identity::accounts::set_ban(&fixture.pool, Some(&admin_session), admin.user.id, true).await,
        Err(identity::accounts::BanError::SelfBan)
    ));
    assert!(
        identity::accounts::set_ban(&fixture.pool, Some(&admin_session), temporary.id, true)
            .await
            .unwrap()
            .is_banned
    );
    let banned_session = random_id();
    assert!(matches!(
        signed_in(
            identity::login::password_login(
                &fixture.pool,
                None,
                PasswordCredentials {
                    identity: "temporary",
                    password: "replacement-password"
                },
                session_context(&banned_session, None),
            )
            .await
        ),
        Err(identity::login::LoginError::Banned)
    ));
}
